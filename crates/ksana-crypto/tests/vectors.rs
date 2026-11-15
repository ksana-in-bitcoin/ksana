//! Check the shared test vectors `tests/vectors/signing.json` (including the BIP322 cases, D-052) and
//! `tests/vectors/merkle.json`.

use ksana_crypto::{
    account_of, aggregate, entries_root, half_signed, keypair_from_secret, merkle_root,
    native_vmop_sighash, seal_entries, sign_native_vmop, sign_vmop, tagged_hash, unseal_entries,
    verify_aggregate, verify_native_vmop, verify_vmop, verify_vmops_aggregate, vmop_sighash,
    HalfSigned, VMOP_SIGHASH_TAG,
};
use ksana_primitives::{Entry, FeeMode, OutPoint, SigScheme, VmOp};
use serde_json::Value;

fn load(name: &str) -> Value {
    let path = format!("{}/../../tests/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn bytes(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().unwrap()).unwrap()
}

fn arr32(v: &Value) -> [u8; 32] {
    bytes(v).try_into().unwrap()
}

#[test]
fn signing_vectors() {
    let v = load("signing.json");
    assert_eq!(v["tag"], VMOP_SIGHASH_TAG);

    for t in v["tagged_hash"].as_array().unwrap() {
        let tag = t["tag"].as_str().unwrap();
        assert_eq!(
            tagged_hash(tag, &bytes(&t["msg"])).to_vec(),
            bytes(&t["hash"])
        );
    }

    let cases = v["cases"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let kp = keypair_from_secret(&arr32(&case["secret_key"])).unwrap();
        assert_eq!(account_of(&kp), arr32(&case["account"]), "{name}: account");

        let op: VmOp = serde_json::from_value(case["op"].clone()).unwrap();
        assert_eq!(op.account, arr32(&case["account"]), "{name}: op.account");
        assert_eq!(
            op.encode_unsigned().unwrap(),
            bytes(&case["unsigned_borsh"]),
            "{name}: signing message content"
        );
        let aux = arr32(&case["aux_rand"]);
        match op.fee {
            FeeMode::Token { .. } => {
                assert!(
                    case.get("first_input").is_none(),
                    "{name}: path B has no first_input"
                );
                assert_eq!(
                    vmop_sighash(&op).unwrap(),
                    arr32(&case["sighash"]),
                    "{name}: sighash"
                );
                assert_eq!(
                    sign_vmop(&op, &kp, &aux).unwrap(),
                    op.sig,
                    "{name}: signature"
                );
                assert_eq!(verify_vmop(&op), Ok(()), "{name}: verification");
            }
            FeeMode::BtcNative => {
                // D-086: the signature is bound to the first input of the carrying transaction
                let input = OutPoint {
                    txid: arr32(&case["first_input"]["txid"]),
                    vout: u32::try_from(case["first_input"]["vout"].as_u64().unwrap()).unwrap(),
                };
                assert_eq!(
                    native_vmop_sighash(&op, &input).unwrap(),
                    arr32(&case["sighash"]),
                    "{name}: sighash"
                );
                assert_eq!(
                    sign_native_vmop(&op, &input, &kp, &aux).unwrap(),
                    op.sig,
                    "{name}: signature"
                );
                assert_eq!(
                    verify_native_vmop(&op, &input),
                    Ok(()),
                    "{name}: verification"
                );
                let other = OutPoint {
                    vout: input.vout + 1,
                    ..input
                };
                assert!(
                    verify_native_vmop(&op, &other).is_err(),
                    "{name}: verification still passes with a different input"
                );
                assert!(
                    verify_vmop(&op).is_err(),
                    "{name}: verification still passes without the input"
                );
            }
        }
        assert_eq!(
            op.encode().unwrap(),
            bytes(&case["borsh"]),
            "{name}: encoding"
        );
        assert_eq!(op.op_id().unwrap(), arr32(&case["op_id"]), "{name}: op_id");
    }

    let original: VmOp = serde_json::from_value(cases[0]["op"].clone()).unwrap();
    let tampered = v["tampered"].as_array().unwrap();
    assert!(!tampered.is_empty());
    for t in tampered {
        let name = t["name"].as_str().unwrap();
        let op: VmOp = serde_json::from_value(t["op"].clone()).unwrap();
        assert_ne!(op, original, "{name}: must differ from the original op");
        assert!(
            verify_vmop(&op).is_err(),
            "{name}: verification still passes after tampering"
        );
    }
}

/// BIP322 simple cases (D-052, D-092): the message, message hash, `to_spend` / `to_sign` transaction IDs and witness
/// are reproducible byte for byte, and the signed VmOp verifies.
#[test]
fn bip322_vectors() {
    use bitcoin::hashes::Hash;
    use ksana_crypto::bip322;
    use ksana_crypto::sign_vmop_bip322_p2tr;

    let v = load("signing.json");
    let cases = v["bip322"].as_array().unwrap();
    assert_eq!(cases.len(), 1);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let kp = keypair_from_secret(&arr32(&case["secret_key"])).unwrap();
        let op: VmOp = serde_json::from_value(case["op"].clone()).unwrap();
        assert_eq!(op.account, account_of(&kp), "{name}: account");
        assert_eq!(op.account, arr32(&case["account"]), "{name}: account");
        let sighash = vmop_sighash(&op).unwrap();
        assert_eq!(sighash, arr32(&case["sighash"]), "{name}: sighash");
        let message = bip322::vmop_message(&sighash);
        assert_eq!(
            message,
            case["message"].as_str().unwrap().as_bytes(),
            "{name}"
        );
        assert_eq!(
            bip322::message_hash(&message),
            arr32(&case["message_hash"]),
            "{name}: message_hash"
        );
        let script = bitcoin::ScriptBuf::from_bytes(bytes(&case["script_pubkey"]));
        let spend = bip322::to_spend(&script, &message);
        assert_eq!(
            spend.compute_txid().to_byte_array(),
            arr32(&case["to_spend_txid"]),
            "{name}: to_spend"
        );
        assert_eq!(case["address_type"].as_str().unwrap(), "p2tr");
        // The account is the address's output public key (D-092)
        assert_eq!(bip322::account_script(&op.account).unwrap(), script);
        assert_eq!(script.as_bytes()[2..], op.account);
        let sig = sign_vmop_bip322_p2tr(&op, &kp, &arr32(&case["aux_rand"])).unwrap();
        assert_eq!(sig, op.sig, "{name}: witness is reproducible");
        let SigScheme::Bip322Simple(witness) = &op.sig else {
            panic!("{name}: must be Bip322Simple");
        };
        assert_eq!(*witness, bytes(&case["witness"]), "{name}: witness");
        let sign = bip322::to_sign(
            spend.compute_txid(),
            bitcoin::consensus::deserialize(witness).unwrap(),
        );
        assert_eq!(
            sign.compute_txid().to_byte_array(),
            arr32(&case["to_sign_txid"]),
            "{name}: to_sign"
        );
        assert_eq!(bip322::verify_simple(&script, &message, witness), Ok(()));
        assert_eq!(verify_vmop(&op), Ok(()), "{name}: verification");
        assert_eq!(
            op.encode().unwrap(),
            bytes(&case["op_borsh"]),
            "{name}: encoding"
        );
        assert_eq!(op.op_id().unwrap(), arr32(&case["op_id"]), "{name}: op_id");
    }
}

/// The aggregate of a block's path B signatures (D-154): the entries a block holds, the second halves beside it, the
/// items the aggregate is over and the aggregate itself follow byte for byte from the entries the accounts signed, and
/// none of the failing cases verifies.
#[test]
fn aggregate_vectors() {
    let v = load("signing.json");
    let case = &v["aggregate"];
    let signed: Vec<Entry> = serde_json::from_value(case["signed"].clone()).unwrap();
    let sealed: Vec<Entry> = serde_json::from_value(case["sealed"].clone()).unwrap();
    let second_halves: Vec<[u8; 32]> = case["second_halves"]
        .as_array()
        .unwrap()
        .iter()
        .map(arr32)
        .collect();
    let s = arr32(&case["s"]);

    assert_eq!(
        seal_entries(&signed).unwrap(),
        (sealed.clone(), second_halves.clone())
    );
    assert_eq!(sealed.last(), Some(&Entry::Aggregate { s }));
    assert_eq!(entries_root(&sealed).unwrap(), arr32(&case["entries_root"]));

    let path_b = |entries: &[Entry]| -> Vec<VmOp> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Sponsored { op } => Some(op.clone()),
                _ => None,
            })
            .collect()
    };
    let whole = path_b(&signed);
    let halves = path_b(&sealed);
    assert!(halves.len() >= 4);
    let forms: std::collections::BTreeSet<_> = halves
        .iter()
        .map(|op| match op.sig {
            SigScheme::HalfBip340(_) => "HalfBip340",
            SigScheme::HalfBip322(_) => "HalfBip322",
            SigScheme::HalfBip322All(_) => "HalfBip322All",
            SigScheme::Bip340(_) | SigScheme::Bip322Simple(_) => "whole",
        })
        .collect();
    assert_eq!(
        forms.into_iter().collect::<Vec<_>>(),
        ["HalfBip322", "HalfBip322All", "HalfBip340"],
        "the three forms of half a signature"
    );
    assert_eq!(
        unseal_entries(&sealed, &second_halves).unwrap(),
        whole.clone()
    );

    let items = case["items"].as_array().unwrap();
    assert_eq!(items.len(), halves.len());
    let mut expected = Vec::new();
    for ((item, half), op) in items.iter().zip(&halves).zip(&whole) {
        verify_vmop(op).unwrap();
        let signed = half_signed(half).unwrap();
        assert_eq!(
            signed,
            HalfSigned {
                pubkey: arr32(&item["pubkey"]),
                msg: arr32(&item["msg"]),
                r: arr32(&item["r"]),
            }
        );
        assert_eq!(signed.pubkey, op.account);
        assert_eq!(half.op_id().unwrap(), arr32(&item["op_id"]));
        assert_eq!(op.op_id().unwrap(), arr32(&item["op_id"]));
        expected.push(signed);
    }
    assert_eq!(aggregate(&expected, &second_halves).unwrap(), s);
    verify_aggregate(&expected, &s).unwrap();
    verify_vmops_aggregate(halves.iter(), &s).unwrap();

    let failing = case["failing"].as_array().unwrap();
    assert!(failing.len() >= 4);
    for case in failing {
        let name = case["name"].as_str().unwrap();
        let ops: Vec<VmOp> = serde_json::from_value(case["ops"].clone()).unwrap();
        assert!(
            verify_vmops_aggregate(ops.iter(), &arr32(&case["s"])).is_err(),
            "{name}: the aggregate still verifies"
        );
    }
}

#[test]
fn merkle_vectors() {
    let v = load("merkle.json");
    let rfc = v["rfc6962"].as_array().unwrap();
    assert_eq!(rfc.len(), 9);
    for case in rfc {
        let leaves: Vec<Vec<u8>> = case["leaves"]
            .as_array()
            .unwrap()
            .iter()
            .map(bytes)
            .collect();
        assert_eq!(merkle_root(&leaves), arr32(&case["root"]));
    }
    for case in v["entries_root"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let entries: Vec<Entry> = serde_json::from_value(case["entries"].clone()).unwrap();
        assert_eq!(
            entries_root(&entries).unwrap(),
            arr32(&case["entries_root"]),
            "{name}"
        );
    }
}
