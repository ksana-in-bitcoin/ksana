//! Adversarial tests: signature schemes, message forms and key forms are not interchangeable (D-052, D-086, D-092).
//!
//! - The account is the BIP86-tweaked output public key: a BIP340 signature for the same account made with the
//!   untweaked internal key is invalid (wallets must sign with the tweaked key).
//! - The BIP322 simple message is the lowercase hex of the signature hash: witnesses signed over the raw 32 bytes,
//!   uppercase hex or a `0x`-prefixed message are all invalid.
//! - A raw 64-byte Schnorr signature is not a witness encoding; wrapped in a witness encoding it still does not sign
//!   BIP322's to_sign transaction, so it is invalid as well.
//! - A path C BIP322 signature is likewise bound to the first input of the carrying transaction.
//! - Arbitrary bytes as a BIP322 witness only fail verification and never panic (complements the fuzz target
//!   `bip322_witness`).

use bitcoin::consensus::encode::serialize;
use bitcoin::Witness;
use ksana_crypto::bip322::{sign_simple_p2tr, verify_for_account, vmop_message};
use ksana_crypto::{
    account_of, keypair_from_secret, native_vmop_sighash, schnorr_sign, sign_native_vmop,
    sign_native_vmop_bip322_p2tr, sign_vmop, tweaked_keypair, verify_native_vmop, verify_vmop,
    vmop_sighash, Keypair, SigError,
};
use ksana_primitives::{
    Action, FeeMode, OutPoint, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use proptest::prelude::*;

fn keypair(byte: u8) -> Keypair {
    keypair_from_secret(&[byte; 32]).unwrap()
}

fn op(account: [u8; 32], fee: FeeMode) -> VmOp {
    VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account,
        nonce: 1,
        fee,
        max_gas: 50_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [0x42; 32],
            amount: 7,
        },
        sig: SigScheme::Bip340([0; 64]),
    }
}

const FIRST_INPUT: OutPoint = OutPoint {
    txid: [0xc1; 32],
    vout: 1,
};

#[test]
fn a_bip340_signature_by_the_untweaked_internal_key_is_rejected() {
    let kp = keypair(21);
    let account = account_of(&kp);
    let internal = kp.x_only_public_key().0.serialize();
    assert_ne!(
        account, internal,
        "the account is the tweaked output public key, not the internal public key"
    );
    let unsigned = op(account, FeeMode::Token { amount: 2 });
    let sighash = vmop_sighash(&unsigned).unwrap();
    // Signing directly with the internal key: invalid for the account
    let by_internal = VmOp {
        sig: SigScheme::Bip340(schnorr_sign(&sighash, &kp, &[1; 32])),
        ..unsigned.clone()
    };
    assert_eq!(verify_vmop(&by_internal), Err(SigError::InvalidSignature));
    // Signing with the tweaked key: valid, and identical to the result of sign_vmop
    let tweaked = tweaked_keypair(&kp).unwrap();
    assert_eq!(tweaked.x_only_public_key().0.serialize(), account);
    let by_tweaked = VmOp {
        sig: SigScheme::Bip340(schnorr_sign(&sighash, &tweaked, &[1; 32])),
        ..unsigned.clone()
    };
    assert_eq!(verify_vmop(&by_tweaked), Ok(()));
    assert_eq!(by_tweaked.sig, sign_vmop(&unsigned, &kp, &[1; 32]).unwrap());
    // The tweaked key cannot in turn be handed to the signing interface as the "local secret key": its account is the
    // public key tweaked once more
    assert_eq!(
        sign_vmop(&unsigned, &tweaked, &[1; 32]),
        Err(SigError::AccountMismatch)
    );
    // Same for path C
    let native = op(account, FeeMode::BtcNative);
    let native_sighash = native_vmop_sighash(&native, &FIRST_INPUT).unwrap();
    let forged = VmOp {
        sig: SigScheme::Bip340(schnorr_sign(&native_sighash, &kp, &[1; 32])),
        ..native.clone()
    };
    assert_eq!(
        verify_native_vmop(&forged, &FIRST_INPUT),
        Err(SigError::InvalidSignature)
    );
    let signed = VmOp {
        sig: sign_native_vmop(&native, &FIRST_INPUT, &kp, &[1; 32]).unwrap(),
        ..native
    };
    assert_eq!(verify_native_vmop(&signed, &FIRST_INPUT), Ok(()));
}

#[test]
fn bip322_witnesses_over_other_message_forms_are_rejected() {
    let kp = keypair(22);
    let account = account_of(&kp);
    let unsigned = op(account, FeeMode::Token { amount: 2 });
    let sighash = vmop_sighash(&unsigned).unwrap();
    let message = vmop_message(&sighash);
    assert_eq!(message, hex::encode(sighash).into_bytes());
    let verify = |witness: Vec<u8>| {
        verify_vmop(&VmOp {
            sig: SigScheme::Bip322Simple(witness),
            ..unsigned.clone()
        })
    };
    // The correct message form
    assert_eq!(
        verify(sign_simple_p2tr(&kp, &message, &[0; 32]).unwrap()),
        Ok(())
    );
    // Raw 32 bytes, uppercase hex, 0x prefix, trailing newline: none is the agreed message
    for wrong in [
        sighash.to_vec(),
        hex::encode_upper(sighash).into_bytes(),
        format!("0x{}", hex::encode(sighash)).into_bytes(),
        format!("{}\n", hex::encode(sighash)).into_bytes(),
    ] {
        assert_eq!(
            verify(sign_simple_p2tr(&kp, &wrong, &[0; 32]).unwrap()),
            Err(SigError::InvalidSignature),
            "message {}",
            hex::encode(&wrong)
        );
    }
}

#[test]
fn a_raw_schnorr_signature_is_not_a_bip322_witness() {
    let kp = keypair(23);
    let account = account_of(&kp);
    let unsigned = op(account, FeeMode::Token { amount: 2 });
    let sighash = vmop_sighash(&unsigned).unwrap();
    let raw = schnorr_sign(&sighash, &tweaked_keypair(&kp).unwrap(), &[0; 32]);
    // Valid as Bip340
    assert_eq!(
        verify_vmop(&VmOp {
            sig: SigScheme::Bip340(raw),
            ..unsigned.clone()
        }),
        Ok(())
    );
    // The same 64 bytes stuffed into Bip322Simple: not a witness encoding; wrapped in a witness encoding, it signs the
    // VmOp signature hash rather than BIP322's to_sign
    for witness in [
        raw.to_vec(),
        serialize(&Witness::from_slice(&[raw.to_vec()])),
    ] {
        assert_eq!(
            verify_vmop(&VmOp {
                sig: SigScheme::Bip322Simple(witness.clone()),
                ..unsigned.clone()
            }),
            Err(SigError::InvalidSignature)
        );
        assert!(verify_for_account(&account, &vmop_message(&sighash), &witness).is_err());
    }
}

#[test]
fn a_native_bip322_signature_is_bound_to_the_first_input() {
    let kp = keypair(24);
    let native = op(account_of(&kp), FeeMode::BtcNative);
    let signed = VmOp {
        sig: sign_native_vmop_bip322_p2tr(&native, &FIRST_INPUT, &kp, &[5; 32]).unwrap(),
        ..native.clone()
    };
    assert!(matches!(signed.sig, SigScheme::Bip322Simple(_)));
    assert_eq!(verify_native_vmop(&signed, &FIRST_INPUT), Ok(()));
    for other in [
        OutPoint {
            vout: 0,
            ..FIRST_INPUT
        },
        OutPoint {
            txid: [0xc2; 32],
            ..FIRST_INPUT
        },
    ] {
        assert_eq!(
            verify_native_vmop(&signed, &other),
            Err(SigError::InvalidSignature),
            "{other:?}"
        );
    }
    // The path B interface does not accept it
    assert_eq!(verify_vmop(&signed), Err(SigError::FirstInputRequired));
}

proptest! {
    #[test]
    fn arbitrary_witness_bytes_never_verify_and_never_panic(
        witness in prop::collection::vec(any::<u8>(), 0..300),
        account_kind in 0u8..3,
    ) {
        let kp = keypair(25);
        let account = match account_kind {
            0 => account_of(&kp),
            1 => [0xff; 32], // not on the curve
            _ => [0; 32],
        };
        prop_assert!(verify_for_account(&account, b"message", &witness).is_err());
        let signed = VmOp {
            sig: SigScheme::Bip322Simple(witness),
            ..op(account, FeeMode::Token { amount: 2 })
        };
        prop_assert!(verify_vmop(&signed).is_err());
    }
}
