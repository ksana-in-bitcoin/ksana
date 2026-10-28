//! BIP322 simple signatures (D-052; since D-092 accounts only correspond to P2TR): the official BIP322 test vectors,
//! signing round trips, tampering and unsupported forms, signing and verification at the VmOp level.
//!
//! The official vectors come from bitcoin/bips' `bip-0322/basic-test-vectors.json` and `generated-test-vectors.json`
//! (`tests/data/bip322/`); newer signatures carry an `smp` / `ful` / `pof` prefix, and a simple signature with `smp`
//! stripped is the base64 witness encoding. One local change (D-099): the third `tx_hashes` case had Chinese words in
//! its message, replaced here by `₿ €`; its three hashes were recomputed with an independent script that reproduces
//! all three upstream cases.

use std::str::FromStr;

use base64::Engine;
use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::Hash;
use bitcoin::key::{Keypair, Secp256k1, TapTweak};
use bitcoin::secp256k1::Message;
use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use bitcoin::{Address, Amount, PrivateKey, ScriptBuf, TxOut, Witness};
use ksana_crypto::bip322::{
    account_script, message_hash, sign_simple_p2tr, to_sign, to_spend, verify_for_account,
    verify_simple, vmop_message, MAX_BIP322_SIG_BYTES,
};
use ksana_crypto::{
    account_of, keypair_from_secret, sign_vmop_bip322_p2tr, verify_vmop, vmop_sighash, SigError,
};
use ksana_primitives::{Action, FeeMode, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION};
use serde_json::Value;

fn vectors(name: &str) -> Value {
    let path = format!("{}/tests/data/bip322/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn script_of(address: &str) -> ScriptBuf {
    Address::from_str(address)
        .unwrap()
        .assume_checked()
        .script_pubkey()
}

/// The bytes of a simple signature: base64-decoded after stripping the optional `smp` prefix; `None` when it is not
/// the simple format (another prefix) or cannot be decoded.
fn simple_bytes(signature: &str) -> Option<Vec<u8>> {
    let body = signature.strip_prefix("smp").unwrap_or(signature);
    if body.starts_with("ful") || body.starts_with("pof") {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

/// The generic `verify_simple` supports P2TR and P2WPKH (D-052); accounts only correspond to P2TR (D-092).
fn supported(kind: &str) -> bool {
    matches!(kind, "p2tr" | "p2wpkh")
}

#[test]
fn official_message_hashes_and_transaction_ids() {
    let v = vectors("basic-test-vectors.json");
    for case in v["tx_hashes"].as_array().unwrap() {
        let message = case["message"].as_str().unwrap().as_bytes();
        assert_eq!(
            hex::encode(message_hash(message)),
            case["message_hash"].as_str().unwrap()
        );
        let spend = to_spend(&script_of(case["address"].as_str().unwrap()), message);
        assert_eq!(
            spend.compute_txid().to_string(),
            case["to_spend_tx_hash"].as_str().unwrap()
        );
        let sign = to_sign(spend.compute_txid(), Witness::new());
        assert_eq!(
            sign.compute_txid().to_string(),
            case["to_sign_tx_hash"].as_str().unwrap()
        );
    }
}

#[test]
fn official_simple_signatures_verify_for_supported_types() {
    let mut checked = 0;
    for file in ["basic-test-vectors.json", "generated-test-vectors.json"] {
        let v = vectors(file);
        for case in v["simple"].as_array().unwrap() {
            let kind = case["type"].as_str().unwrap();
            let script = script_of(case["address"].as_str().unwrap());
            let message = case["message"].as_str().unwrap().as_bytes();
            for signature in case["bip322_signatures"].as_array().unwrap() {
                let bytes = simple_bytes(signature.as_str().unwrap()).unwrap();
                let result = verify_simple(&script, message, &bytes);
                if supported(kind) {
                    assert_eq!(result, Ok(()), "{file} {kind} {}", case["address"]);
                    checked += 1;
                } else {
                    // Other script types such as P2WSH are unsupported: verification always fails
                    assert!(result.is_err(), "{file} {kind} must not pass");
                }
            }
            if supported(kind) {
                // Verify by account: the account of P2TR is the address's output public key (the x-only public key of
                // the BIP86-tweaked secret key, D-092); P2WPKH corresponds to no account, so verifying by account
                // always fails
                let wif = case["private_keys"][0].as_str().unwrap();
                let secret = PrivateKey::from_wif(wif).unwrap().inner;
                let keypair = Keypair::from_secret_key(&Secp256k1::new(), &secret);
                let account = account_of(&keypair);
                for signature in case["bip322_signatures"].as_array().unwrap() {
                    let bytes = simple_bytes(signature.as_str().unwrap()).unwrap();
                    if kind == "p2tr" {
                        assert_eq!(
                            account_script(&account).unwrap(),
                            script,
                            "the account's address"
                        );
                        assert_eq!(verify_for_account(&account, message, &bytes), Ok(()));
                    } else {
                        assert_eq!(
                            verify_for_account(&account, message, &bytes),
                            Err(SigError::InvalidSignature),
                            "a P2WPKH witness corresponds to no account (D-092)"
                        );
                    }
                }
            }
        }
    }
    assert!(checked >= 6, "checked {checked} P2TR / P2WPKH signatures");
}

#[test]
fn official_error_cases_fail() {
    for file in ["basic-test-vectors.json", "generated-test-vectors.json"] {
        let v = vectors(file);
        for case in v["error"].as_array().unwrap() {
            let address = case["address"].as_str().unwrap();
            let Ok(address) = Address::from_str(address) else {
                continue;
            };
            let script = address.assume_checked().script_pubkey();
            let message = case["message"].as_str().unwrap().as_bytes();
            let signature = case["signature"].as_str().unwrap();
            if let Some(bytes) = simple_bytes(signature) {
                assert!(
                    verify_simple(&script, message, &bytes).is_err(),
                    "{file}: {}",
                    case["description"]
                );
            }
        }
    }
}

fn keypair(byte: u8) -> Keypair {
    keypair_from_secret(&[byte; 32]).unwrap()
}

#[test]
fn round_trips_for_p2tr_and_rejects_two_item_witnesses() {
    let kp = keypair(7);
    let account = account_of(&kp);
    for message in [b"".as_slice(), b"Hello World", &[0xab; 64]] {
        let tr = sign_simple_p2tr(&kp, message, &[1; 32]).unwrap();
        assert_eq!(verify_for_account(&account, message, &tr), Ok(()));
        // Another account, another message
        let other = account_of(&keypair(8));
        assert!(verify_for_account(&other, message, &tr).is_err());
        assert!(verify_for_account(&account, b"other", &tr).is_err());
        // A two-item witness (the P2WPKH form of D-052) no longer corresponds to an account (D-092)
        let two = serialize(&Witness::from_slice(&[
            vec![0x30u8; 72],
            kp.public_key().serialize().to_vec(),
        ]));
        assert_eq!(
            verify_for_account(&account, message, &two),
            Err(SigError::InvalidSignature)
        );
    }
    // Signing is deterministic
    assert_eq!(
        sign_simple_p2tr(&kp, b"m", &[1; 32]).unwrap(),
        sign_simple_p2tr(&kp, b"m", &[1; 32]).unwrap()
    );
    // An account not on the curve, such as a contract ID, has no address: verification fails rather than panicking
    assert_eq!(
        verify_for_account(
            &[0xff; 32],
            b"m",
            &sign_simple_p2tr(&kp, b"m", &[1; 32]).unwrap()
        ),
        Err(SigError::InvalidPublicKey)
    );
}

/// Sign with `kp`'s BIP86-tweaked key using `hash_type`, returning the witness encoding (the type byte is appended
/// when `hash_type` is not DEFAULT).
fn p2tr_with_type(kp: &Keypair, message: &[u8], hash_type: TapSighashType) -> Vec<u8> {
    let secp = Secp256k1::new();
    let challenge = account_script(&account_of(kp)).unwrap();
    let spend = to_spend(&challenge, message);
    let tx = to_sign(spend.compute_txid(), Witness::new());
    let prevout = TxOut {
        value: Amount::ZERO,
        script_pubkey: challenge,
    };
    let sighash = SighashCache::new(&tx)
        .taproot_key_spend_signature_hash(0, &Prevouts::All(&[prevout]), hash_type)
        .unwrap();
    let tweaked = kp.tap_tweak(&secp, None).to_keypair();
    let sig = secp.sign_schnorr_with_aux_rand(
        &Message::from_digest(sighash.to_byte_array()),
        &tweaked,
        &[0; 32],
    );
    let mut element = sig.as_ref().to_vec();
    if hash_type != TapSighashType::Default {
        element.push(hash_type as u8);
    }
    serialize(&Witness::from_slice(&[element]))
}

#[test]
fn taproot_sighash_types() {
    let kp = keypair(9);
    let account = account_of(&kp);
    // SIGHASH_ALL (65 bytes, last byte 0x01) is accepted
    let all = p2tr_with_type(&kp, b"m", TapSighashType::All);
    assert_eq!(verify_for_account(&account, b"m", &all), Ok(()));
    // Other types are rejected
    for t in [
        TapSighashType::None,
        TapSighashType::Single,
        TapSighashType::AllPlusAnyoneCanPay,
        TapSighashType::SinglePlusAnyoneCanPay,
    ] {
        let sig = p2tr_with_type(&kp, b"m", t);
        assert_eq!(
            verify_for_account(&account, b"m", &sig),
            Err(SigError::InvalidSignature),
            "{t:?}"
        );
    }
    // A 64-byte signature followed by 0x00 (the explicit spelling of DEFAULT) is rejected too: only 64 bytes, or 65
    // bytes with SIGHASH_ALL, are accepted
    let mut explicit = p2tr_with_type(&kp, b"m", TapSighashType::Default);
    explicit[0] = 1;
    explicit[1] = 65;
    explicit.push(0);
    assert!(verify_for_account(&account, b"m", &explicit).is_err());
}

#[test]
fn malformed_and_unsupported_witnesses_fail() {
    let kp = keypair(10);
    let account = account_of(&kp);
    let tr = sign_simple_p2tr(&kp, b"m", &[0; 32]).unwrap();
    assert_eq!(verify_for_account(&account, b"m", &tr), Ok(()));

    // Trailing bytes, truncation, an empty witness, two-item and three-item witnesses
    for sig in [
        [tr.clone(), vec![0]].concat(),
        tr[..tr.len() - 1].to_vec(),
        vec![0],
        vec![],
        serialize(&Witness::from_slice(&[vec![1u8; 72], vec![2; 33]])),
        serialize(&Witness::from_slice(&[
            vec![1u8; 64],
            vec![2; 33],
            vec![3; 1],
        ])),
    ] {
        assert!(verify_for_account(&account, b"m", &sig).is_err());
    }
    // Over the length limit
    let long = serialize(&Witness::from_slice(&[vec![0u8; MAX_BIP322_SIG_BYTES]]));
    assert!(verify_for_account(&account, b"m", &long).is_err());

    // P2TR: tampered signature, wrong signature length
    let mut bad = tr.clone();
    bad[5] ^= 1;
    assert!(verify_for_account(&account, b"m", &bad).is_err());
    for len in [63usize, 66] {
        let sig = serialize(&Witness::from_slice(&[vec![7u8; len]]));
        assert_eq!(
            verify_for_account(&account, b"m", &sig),
            Err(SigError::InvalidSignature)
        );
    }
}

fn sample_op(account: [u8; 32]) -> VmOp {
    VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account,
        nonce: 3,
        fee: FeeMode::Token { amount: 2 },
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [9; 32],
            amount: 5,
        },
        sig: SigScheme::Bip340([0; 64]),
    }
}

#[test]
fn vmop_message_is_the_lowercase_hex_of_the_sighash() {
    let op = sample_op(account_of(&keypair(12)));
    let sighash = vmop_sighash(&op).unwrap();
    assert_eq!(vmop_message(&sighash), hex::encode(sighash).into_bytes());
}

#[test]
fn vmops_signed_with_bip322_verify_and_tampering_fails() {
    let kp = keypair(13);
    let op = sample_op(account_of(&kp));
    for sig in [sign_vmop_bip322_p2tr(&op, &kp, &[4; 32]).unwrap()] {
        let signed = VmOp {
            sig: sig.clone(),
            ..op.clone()
        };
        assert_eq!(verify_vmop(&signed), Ok(()));
        // Tampering with any field covered by the signature makes verification fail
        let tampered = [
            VmOp {
                nonce: 4,
                ..signed.clone()
            },
            VmOp {
                chain_id: 1002,
                ..signed.clone()
            },
            VmOp {
                max_gas: 100_001,
                ..signed.clone()
            },
            VmOp {
                fee: FeeMode::Token { amount: 3 },
                ..signed.clone()
            },
            VmOp {
                account: account_of(&keypair(14)),
                ..signed.clone()
            },
        ];
        for t in tampered {
            assert!(verify_vmop(&t).is_err());
        }
    }
    // Signing is refused when the account does not match the signing secret key
    assert_eq!(
        sign_vmop_bip322_p2tr(&op, &keypair(15), &[0; 32]),
        Err(SigError::AccountMismatch)
    );
}
