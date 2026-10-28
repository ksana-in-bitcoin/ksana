//! Checks the shared test vectors `tests/vectors/codec.json` (M0 acceptance item 1; the TS SDK uses the same file in
//! M6).
//!
//! For each valid case: JSON value → Borsh must equal the bytes in the vector, bytes → value must restore it,
//! the JSON representation must be identical character for character; the VmOp op_id and the header block_hash must
//! match.
//! For each invalid case: decoding must fail; when `error` is given the error message must contain it.

use std::collections::BTreeSet;
use std::fmt::Debug;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_primitives::codec::{decode, encode};
use ksana_primitives::{
    Action, Block, BlockHeader, BtcAnchor, Entry, EntryData, FeeMode, OutPoint, Payload, SigScheme,
    VmOp, DEVNET,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

fn load(name: &str) -> Value {
    let path = format!("{}/../../tests/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn hex_field(case: &Value, key: &str) -> Vec<u8> {
    hex::decode(case[key].as_str().unwrap()).unwrap()
}

fn check_valid<T>(case: &Value) -> T
where
    T: Serialize + DeserializeOwned + BorshSerialize + BorshDeserialize + PartialEq + Debug,
{
    let name = case["name"].as_str().unwrap();
    let value: T = serde_json::from_value(case["value"].clone()).unwrap();
    let bytes = hex_field(case, "borsh");
    assert_eq!(encode(&value).unwrap(), bytes, "{name}: encoding");
    assert_eq!(
        decode::<T>(&bytes, usize::MAX).unwrap(),
        value,
        "{name}: decoding"
    );
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        case["value"],
        "{name}: JSON"
    );
    value
}

/// The variant name of an enum's JSON (externally tagged form: the object's only key, or the string of a unit
/// variant).
fn variant_name(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            assert_eq!(map.len(), 1);
            map.keys().next().unwrap().clone()
        }
        other => panic!("not the JSON representation of an enum: {other}"),
    }
}

#[test]
fn codec_vectors_valid() {
    let vectors = load("codec.json");
    let cases = vectors["valid"].as_array().unwrap();
    let mut types = BTreeSet::new();
    let mut variants: BTreeSet<(String, String)> = BTreeSet::new();

    for case in cases {
        let ty = case["type"].as_str().unwrap();
        types.insert(ty.to_string());
        if matches!(
            ty,
            "FeeMode" | "Action" | "SigScheme" | "Entry" | "EntryData" | "Payload"
        ) {
            variants.insert((ty.to_string(), variant_name(&case["value"])));
        }
        match ty {
            "OutPoint" => {
                check_valid::<OutPoint>(case);
            }
            "FeeMode" => {
                check_valid::<FeeMode>(case);
            }
            "Action" => {
                check_valid::<Action>(case);
            }
            "SigScheme" => {
                check_valid::<SigScheme>(case);
            }
            "VmOp" => {
                let op = check_valid::<VmOp>(case);
                let bytes = hex_field(case, "borsh");
                assert_eq!(VmOp::decode(&bytes, &DEVNET).unwrap(), op);
                assert_eq!(op.op_id().unwrap().to_vec(), hex_field(case, "op_id"));
                assert_eq!(
                    ksana_primitives::hash::sha256(&bytes).to_vec(),
                    hex_field(case, "op_id")
                );
            }
            "BtcAnchor" => {
                check_valid::<BtcAnchor>(case);
            }
            "BlockHeader" => {
                let h = check_valid::<BlockHeader>(case);
                assert_eq!(h.to_bytes().to_vec(), hex_field(case, "borsh"));
                assert_eq!(h.hash().to_vec(), hex_field(case, "block_hash"));
            }
            "Entry" => {
                check_valid::<Entry>(case);
            }
            "EntryData" => {
                check_valid::<EntryData>(case);
            }
            "Block" => {
                check_valid::<Block>(case);
            }
            "Payload" => {
                // Payloads have their own encoding (compressed headers and entries, D-075, D-079, D-080), with the
                // devnet chain ID
                let name = case["name"].as_str().unwrap();
                let p: Payload = serde_json::from_value(case["value"].clone()).unwrap();
                let bytes = hex_field(case, "borsh");
                assert_eq!(p.encode(&DEVNET).unwrap(), bytes, "{name}: encoding");
                assert_eq!(
                    Payload::decode(&bytes, &DEVNET).unwrap(),
                    p,
                    "{name}: decoding"
                );
                assert_eq!(
                    serde_json::to_value(&p).unwrap(),
                    case["value"],
                    "{name}: JSON"
                );
            }
            other => panic!("unknown type {other}"),
        }
    }

    // Every consensus type has vectors, and every enum variant is covered.
    let expected_types = [
        "Action",
        "Block",
        "BlockHeader",
        "BtcAnchor",
        "Entry",
        "EntryData",
        "FeeMode",
        "OutPoint",
        "Payload",
        "SigScheme",
        "VmOp",
    ];
    assert_eq!(
        types,
        expected_types
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>()
    );
    let expected_variants = [
        ("FeeMode", ["Token", "BtcNative"].as_slice()),
        ("Action", &["Deploy", "Call", "AssetTransfer"]),
        ("SigScheme", &["Bip340", "Bip322Simple"]),
        ("Entry", &["Forced", "Sponsored", "Unlock"]),
        ("EntryData", &["Forced", "Sponsored", "Unlock"]),
        ("Payload", &["Settlement", "NativeOp", "Headers"]),
    ];
    for (ty, names) in expected_variants {
        for name in names {
            assert!(
                variants.contains(&(ty.to_string(), name.to_string())),
                "missing vectors for {ty}::{name}"
            );
        }
    }
}

#[test]
fn codec_vectors_invalid() {
    let vectors = load("codec.json");
    let cases = vectors["invalid"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let bytes = hex_field(case, "borsh");
        let error = match case["type"].as_str().unwrap() {
            "VmOp" => VmOp::decode(&bytes, &DEVNET).err(),
            "Block" => decode::<Block>(&bytes, usize::MAX).err(),
            "Entry" => decode::<Entry>(&bytes, usize::MAX).err(),
            "EntryData" => decode::<EntryData>(&bytes, usize::MAX).err(),
            "Payload" => Payload::decode(&bytes, &DEVNET).err(),
            other => panic!("unknown type {other}"),
        };
        let error =
            error.unwrap_or_else(|| panic!("{name}: invalid encoding decoded successfully"));
        // When error is given, rejected by the specified rule (the same error message as the TS SDK, D-075)
        if let Some(expected) = case.get("error").and_then(Value::as_str) {
            assert!(
                error.to_string().contains(expected),
                "{name}: error message \"{error}\" does not contain \"{expected}\""
            );
        }
    }
}
