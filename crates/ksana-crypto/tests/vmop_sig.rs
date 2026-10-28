//! M0 acceptance item: VmOp signing and verification pass, and verification fails after tampering with any field
//! (SPEC §4.2, §14). Path C ops are signed and verified against the first input of the carrying transaction (D-086):
//! a different input makes verification fail, and the path B / C interfaces cannot be mixed.

use ksana_crypto::{
    account_of, keypair_from_secret, native_vmop_sighash, sign_native_vmop, sign_vmop,
    verify_native_vmop, verify_vmop, vmop_sighash, Keypair, SigError,
};
use ksana_primitives::{
    strategies, Action, FeeMode, Funds, OutPoint, SigScheme, VmOp, DEVNET, KSA_ASSET_ID,
    VMOP_VERSION,
};
use proptest::prelude::*;

/// The first input of the carrying transaction that path C ops are bound to.
const FIRST_INPUT: OutPoint = OutPoint {
    txid: [0xc1; 32],
    vout: 1,
};

fn keypair(seed: u8) -> Keypair {
    keypair_from_secret(&[seed; 32]).unwrap()
}

/// Set `op.account` to the keypair's account and sign: path C ops are bound to [`FIRST_INPUT`].
fn signed(mut op: VmOp, kp: &Keypair, aux: &[u8; 32]) -> VmOp {
    op.account = account_of(kp);
    op.sig = match op.fee {
        FeeMode::Token { .. } => sign_vmop(&op, kp, aux).unwrap(),
        FeeMode::BtcNative => sign_native_vmop(&op, &FIRST_INPUT, kp, aux).unwrap(),
    };
    op
}

/// Verify according to the op's fee mode: path C ops against [`FIRST_INPUT`].
fn verify(op: &VmOp) -> Result<(), SigError> {
    match op.fee {
        FeeMode::Token { .. } => verify_vmop(op),
        FeeMode::BtcNative => verify_native_vmop(op, &FIRST_INPUT),
    }
}

/// Verify the tampered op according to the **original** op's fee mode: an op whose fee variant was changed is still
/// verified with the original interface.
fn verify_as(original: &VmOp, tampered: &VmOp) -> Result<(), SigError> {
    match original.fee {
        FeeMode::Token { .. } => verify_vmop(tampered),
        FeeMode::BtcNative => verify_native_vmop(tampered, &FIRST_INPUT),
    }
}

fn fee_modes() -> Vec<FeeMode> {
    vec![FeeMode::Token { amount: 10 }, FeeMode::BtcNative]
}

fn actions() -> Vec<Action> {
    vec![
        Action::Deploy {
            code: vec![0x00, 0x61, 0x73, 0x6d],
            init_args: vec![0x01],
            funds: vec![Funds {
                asset: KSA_ASSET_ID,
                amount: 3,
            }],
        },
        Action::Call {
            contract: [0x55; 32],
            method: "transfer".to_string(),
            args: vec![0xAA],
            funds: vec![Funds {
                asset: [0x66; 32],
                amount: 9,
            }],
        },
        Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [0x22; 32],
            amount: 1_000,
        },
    ]
}

/// Unsigned ops covering every combination of FeeMode × Action variants.
fn base_ops() -> Vec<VmOp> {
    let mut ops = Vec::new();
    for fee in fee_modes() {
        for action in actions() {
            ops.push(VmOp {
                version: VMOP_VERSION,
                chain_id: DEVNET.chain_id,
                account: [0; 32],
                nonce: 3,
                fee: fee.clone(),
                max_gas: 50_000,
                action,
                sig: SigScheme::Bip340([0; 64]),
            });
        }
    }
    ops
}

fn flip(bytes: &mut [u8]) {
    bytes[0] ^= 0x01;
}

fn tweak(bytes: &mut Vec<u8>) {
    match bytes.first_mut() {
        Some(b) => *b ^= 0x01,
        None => bytes.push(0),
    }
}

/// Change the attached funds: add 1 to the first item's amount, or add an item when there is none.
fn tweak_funds(funds: &mut Vec<Funds>) {
    match funds.first_mut() {
        Some(f) => f.amount = f.amount.wrapping_add(1),
        None => funds.push(Funds {
            asset: KSA_ASSET_ID,
            amount: 1,
        }),
    }
}

/// Make one change to each field of `op` (including nested fields and enum variants), returning every single-field
/// tampering result.
fn tampers(op: &VmOp) -> Vec<(String, VmOp)> {
    let mut out: Vec<(String, VmOp)> = Vec::new();
    let mut push = |name: &str, f: &dyn Fn(&mut VmOp)| {
        let mut t = op.clone();
        f(&mut t);
        assert_ne!(&t, op, "tampering with {name} must really change the op");
        out.push((name.to_string(), t));
    };

    push("version", &|t| t.version = t.version.wrapping_add(1));
    push("chain_id", &|t| t.chain_id = t.chain_id.wrapping_add(1));
    push("account", &|t| flip(&mut t.account));
    push("nonce", &|t| t.nonce = t.nonce.wrapping_add(1));
    push("max_gas", &|t| t.max_gas = t.max_gas.wrapping_add(1));

    for (i, fee) in fee_modes().into_iter().enumerate() {
        if std::mem::discriminant(&fee) != std::mem::discriminant(&op.fee) {
            push(&format!("fee variant -> #{i}"), &|t| t.fee = fee.clone());
        }
    }
    match &op.fee {
        FeeMode::Token { .. } => push("fee.amount", &|t| {
            if let FeeMode::Token { amount } = &mut t.fee {
                *amount = amount.wrapping_add(1);
            }
        }),
        FeeMode::BtcNative => {}
    }

    for (i, action) in actions().into_iter().enumerate() {
        if std::mem::discriminant(&action) != std::mem::discriminant(&op.action) {
            push(&format!("action variant -> #{i}"), &|t| {
                t.action = action.clone()
            });
        }
    }
    match &op.action {
        Action::Deploy { .. } => {
            push("action.code", &|t| {
                if let Action::Deploy { code, .. } = &mut t.action {
                    tweak(code);
                }
            });
            push("action.init_args", &|t| {
                if let Action::Deploy { init_args, .. } = &mut t.action {
                    tweak(init_args);
                }
            });
            push("action.funds", &|t| {
                if let Action::Deploy { funds, .. } = &mut t.action {
                    tweak_funds(funds);
                }
            });
        }
        Action::Call { .. } => {
            push("action.contract", &|t| {
                if let Action::Call { contract, .. } = &mut t.action {
                    flip(contract);
                }
            });
            push("action.method", &|t| {
                if let Action::Call { method, .. } = &mut t.action {
                    method.push('x');
                }
            });
            push("action.args", &|t| {
                if let Action::Call { args, .. } = &mut t.action {
                    tweak(args);
                }
            });
            push("action.funds", &|t| {
                if let Action::Call { funds, .. } = &mut t.action {
                    tweak_funds(funds);
                }
            });
        }
        Action::AssetTransfer { .. } => {
            push("action.asset", &|t| {
                if let Action::AssetTransfer { asset, .. } = &mut t.action {
                    flip(asset);
                }
            });
            push("action.to", &|t| {
                if let Action::AssetTransfer { to, .. } = &mut t.action {
                    flip(to);
                }
            });
            push("action.amount", &|t| {
                if let Action::AssetTransfer { amount, .. } = &mut t.action {
                    *amount = amount.wrapping_add(1);
                }
            });
        }
    }

    if let SigScheme::Bip340(_) = &op.sig {
        push("sig bytes", &|t| {
            if let SigScheme::Bip340(sig) = &mut t.sig {
                flip(sig);
            }
        });
        push("sig scheme -> Bip322Simple", &|t| {
            if let SigScheme::Bip340(sig) = &t.sig {
                t.sig = SigScheme::Bip322Simple(sig.to_vec());
            }
        });
    }
    out
}

#[test]
fn sign_and_verify_all_variants() {
    let kp = keypair(7);
    for op in base_ops() {
        let op = signed(op, &kp, &[0x5A; 32]);
        assert_eq!(verify(&op), Ok(()), "{op:?}");
    }
}

/// D-086: a path C signature is bound to the first input of the carrying transaction — verification fails with a
/// different input (another transaction); the path B / C interfaces cannot be mixed.
#[test]
fn native_signatures_are_bound_to_the_first_input() {
    let kp = keypair(8);
    let native = base_ops()
        .into_iter()
        .find(|op| op.fee == FeeMode::BtcNative)
        .unwrap();
    let token = base_ops()
        .into_iter()
        .find(|op| op.fee != FeeMode::BtcNative)
        .unwrap();
    let native = signed(native, &kp, &[1; 32]);
    let token = signed(token, &kp, &[1; 32]);
    assert_eq!(verify_native_vmop(&native, &FIRST_INPUT), Ok(()));
    for other in [
        OutPoint {
            vout: 2,
            ..FIRST_INPUT
        },
        OutPoint {
            txid: [0xc2; 32],
            ..FIRST_INPUT
        },
    ] {
        assert_eq!(
            verify_native_vmop(&native, &other),
            Err(SigError::InvalidSignature),
            "{other:?}"
        );
        assert_ne!(
            native_vmop_sighash(&native, &other).unwrap(),
            native_vmop_sighash(&native, &FIRST_INPUT).unwrap()
        );
    }
    // The message ends with the 36-byte encoding of the first input (txid ‖ vout little-endian)
    let bytes = FIRST_INPUT.to_bytes();
    assert_eq!(bytes[..32], [0xc1; 32]);
    assert_eq!(bytes[32..], 1u32.to_le_bytes());
    // The interfaces cannot be mixed
    assert_eq!(verify_vmop(&native), Err(SigError::FirstInputRequired));
    assert_eq!(vmop_sighash(&native), Err(SigError::FirstInputRequired));
    assert_eq!(
        sign_vmop(&native, &kp, &[1; 32]),
        Err(SigError::FirstInputRequired)
    );
    assert_eq!(
        verify_native_vmop(&token, &FIRST_INPUT),
        Err(SigError::UnexpectedFirstInput)
    );
    assert_eq!(
        native_vmop_sighash(&token, &FIRST_INPUT),
        Err(SigError::UnexpectedFirstInput)
    );
    assert_eq!(
        sign_native_vmop(&token, &FIRST_INPUT, &kp, &[1; 32]),
        Err(SigError::UnexpectedFirstInput)
    );
}

#[test]
fn tampering_any_field_fails_verification() {
    let kp = keypair(7);
    let mut checked = 0;
    for op in base_ops() {
        let op = signed(op, &kp, &[0x5A; 32]);
        for (name, tampered) in tampers(&op) {
            assert!(
                verify_as(&op, &tampered).is_err(),
                "verification still passes after tampering with {name}: {tampered:?}"
            );
            // An op whose fee variant was changed also fails verification with the new variant's interface (the
            // signing message differs)
            assert!(
                verify(&tampered).is_err(),
                "verification by the new fee mode still passes after tampering with {name}: {tampered:?}"
            );
            checked += 1;
        }
    }
    // Every op has 10 generic tamperings: 5 scalar fields + 1 fee variant change + 2 action variant changes + 2 on
    // the signature; plus the fee sub-fields (Token 1, BtcNative 0) and the action sub-fields (Deploy 3, Call 4,
    // AssetTransfer 3). 2 fee modes × 3 actions make 6 combinations, in total 6×10 + 3×(1+0) + 2×(3+4+3) = 83.
    assert_eq!(checked, 83);
}

#[test]
fn signature_does_not_cover_sig_field() {
    let kp = keypair(9);
    let op = signed(base_ops().remove(0), &kp, &[1; 32]);
    let mut other = op.clone();
    other.sig = SigScheme::Bip322Simple(vec![1, 2, 3]);
    assert_eq!(vmop_sighash(&op), vmop_sighash(&other));
}

#[test]
fn signing_is_deterministic_given_aux_rand() {
    let kp = keypair(11);
    let a = signed(base_ops().remove(4), &kp, &[3; 32]);
    let b = signed(base_ops().remove(4), &kp, &[3; 32]);
    let c = signed(base_ops().remove(4), &kp, &[4; 32]);
    assert_eq!(a.sig, b.sig);
    assert_ne!(a.sig, c.sig);
    assert_eq!(verify(&c), Ok(()));
}

#[test]
fn signing_requires_matching_account() {
    let op = VmOp {
        account: account_of(&keypair(1)),
        ..base_ops().remove(0)
    };
    assert_eq!(
        sign_vmop(&op, &keypair(2), &[0; 32]),
        Err(SigError::AccountMismatch)
    );
}

#[test]
fn malformed_bip322_simple_signature_fails() {
    // Valid BIP322 signatures are in tests/bip322.rs (D-052); here we only check that arbitrary bytes do not pass
    for sig in [vec![0; 65], vec![], vec![1, 64]] {
        let op = VmOp {
            sig: SigScheme::Bip322Simple(sig),
            ..signed(base_ops().remove(0), &keypair(5), &[0; 32])
        };
        assert_eq!(verify_vmop(&op), Err(SigError::InvalidSignature));
    }
}

#[test]
fn signature_from_another_account_fails() {
    let op = signed(base_ops().remove(2), &keypair(5), &[0; 32]);
    let forged = VmOp {
        account: account_of(&keypair(6)),
        ..op
    };
    assert_eq!(verify(&forged), Err(SigError::InvalidSignature));
}

fn secret_key() -> impl Strategy<Value = [u8; 32]> {
    any::<[u8; 32]>().prop_filter("valid secret key", |sk| keypair_from_secret(sk).is_ok())
}

proptest! {
    /// Ops with arbitrary field values: verification passes after signing; it fails after any single-field tampering.
    #[test]
    fn arbitrary_ops_sign_verify_and_detect_tampering(
        op in strategies::vmop(),
        sk in secret_key(),
        aux in any::<[u8; 32]>(),
        pick in any::<prop::sample::Index>(),
    ) {
        let kp = keypair_from_secret(&sk).unwrap();
        let op = signed(op, &kp, &aux);
        prop_assert_eq!(verify(&op), Ok(()));
        let all = tampers(&op);
        let (name, tampered) = &all[pick.index(all.len())];
        prop_assert!(verify_as(&op, tampered).is_err(), "verification still passes after tampering with {}", name);
        prop_assert!(verify(tampered).is_err(), "verification by the new fee mode still passes after tampering with {}", name);
    }
}
