//! The aggregate of a block's path B signatures (SPEC §4.5, D-154): the three forms of signature a path B op may carry
//! enter one aggregate, the sealed entries verify exactly when every signature does, and the second halves give the
//! signed ops back.

use ksana_crypto::bip322::{account_sighash, vmop_message};
use ksana_crypto::{
    account_of, half_signed, keypair_from_secret, schnorr_sign, seal_entries, sign_vmop,
    sign_vmop_bip322_p2tr, tweaked_keypair, unseal_entries, verify_vmop, verify_vmops_aggregate,
    vmop_sighash, Keypair, SigError,
};
use ksana_primitives::{
    Action, Entry, FeeMode, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};

fn keypair(seed: u8) -> Keypair {
    keypair_from_secret(&[seed; 32]).unwrap()
}

fn unsigned(kp: &Keypair, nonce: u64) -> VmOp {
    VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: account_of(kp),
        nonce,
        fee: FeeMode::Token { amount: 10 },
        max_gas: 50_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [0x77; 32],
            amount: 1,
        },
        sig: SigScheme::Bip340([0; 64]),
    }
}

fn bip340(seed: u8, nonce: u64) -> VmOp {
    let kp = keypair(seed);
    let mut op = unsigned(&kp, nonce);
    op.sig = sign_vmop(&op, &kp, &[1; 32]).unwrap();
    op
}

fn bip322(seed: u8, nonce: u64) -> VmOp {
    let kp = keypair(seed);
    let mut op = unsigned(&kp, nonce);
    op.sig = sign_vmop_bip322_p2tr(&op, &kp, &[2; 32]).unwrap();
    op
}

/// A BIP322 signature with `SIGHASH_ALL`, as some wallets make it: a witness of one item of 65 bytes.
fn bip322_all(seed: u8, nonce: u64) -> VmOp {
    let kp = keypair(seed);
    let mut op = unsigned(&kp, nonce);
    let message = vmop_message(&vmop_sighash(&op).unwrap());
    let digest = account_sighash(&op.account, &message, true).unwrap();
    let sig = schnorr_sign(&digest, &tweaked_keypair(&kp).unwrap(), &[3; 32]);
    let mut witness = vec![1, 65];
    witness.extend(sig);
    witness.push(1);
    op.sig = SigScheme::Bip322Simple(witness);
    op
}

fn ops() -> Vec<VmOp> {
    vec![
        bip340(1, 0),
        bip322(2, 0),
        bip322_all(3, 0),
        bip340(1, 1),
        bip322(2, 1),
    ]
}

fn entries(ops: &[VmOp]) -> Vec<Entry> {
    let mut entries = vec![Entry::Unlock {
        height: 1,
        index: 0,
    }];
    entries.extend(ops.iter().cloned().map(|op| Entry::Sponsored { op }));
    entries
}

fn sponsored(entries: &[Entry]) -> Vec<&VmOp> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Sponsored { op } => Some(op),
            _ => None,
        })
        .collect()
}

fn aggregate(entries: &[Entry]) -> [u8; 32] {
    match entries.last() {
        Some(Entry::Aggregate { s }) => *s,
        other => panic!("no aggregate entry: {other:?}"),
    }
}

#[test]
fn the_three_forms_of_signature_enter_one_aggregate() {
    let ops = ops();
    for op in &ops {
        verify_vmop(op).unwrap();
    }
    let (sealed, seconds) = seal_entries(&entries(&ops)).unwrap();
    assert_eq!(sealed.len(), ops.len() + 2, "the aggregate entry is added");
    assert_eq!(seconds.len(), ops.len());
    let halves = sponsored(&sealed);
    assert!(matches!(halves[0].sig, SigScheme::HalfBip340(_)));
    assert!(matches!(halves[1].sig, SigScheme::HalfBip322(_)));
    assert!(matches!(halves[2].sig, SigScheme::HalfBip322All(_)));
    for (half, op) in halves.iter().zip(&ops) {
        assert_eq!(half.op_id().unwrap(), op.op_id().unwrap());
        assert_eq!(half_signed(half).unwrap(), half_signed(op).unwrap());
        // Half a signature proves nothing alone
        assert_eq!(verify_vmop(half), Err(SigError::InvalidSignature));
    }
    verify_vmops_aggregate(halves.iter().copied(), &aggregate(&sealed)).unwrap();
    // The second halves give the signed ops back
    assert_eq!(unseal_entries(&sealed, &seconds), Some(ops.clone()));
    assert_eq!(unseal_entries(&sealed, &seconds[1..]), None);
    assert_eq!(unseal_entries(&sealed, &[]), None);
}

#[test]
fn entries_without_path_b_get_no_aggregate() {
    let plain = entries(&[]);
    assert_eq!(seal_entries(&plain).unwrap(), (plain, vec![]));
    assert_eq!(seal_entries(&[]).unwrap(), (vec![], vec![]));
    verify_vmops_aggregate([], &[0; 32]).unwrap();
}

#[test]
fn the_aggregate_fails_when_one_signature_or_anything_it_signs_changes() {
    let ops = ops();
    let (sealed, _) = seal_entries(&entries(&ops)).unwrap();
    let s = aggregate(&sealed);
    let halves: Vec<VmOp> = sponsored(&sealed).into_iter().cloned().collect();
    let check = |ops: &[VmOp], s: &[u8; 32]| verify_vmops_aggregate(ops.iter(), s);
    check(&halves, &s).unwrap();

    // One invalid signature among valid ones, in each form
    for bad in 0..ops.len() {
        let mut ops = ops.clone();
        ops[bad].nonce += 100;
        assert!(verify_vmop(&ops[bad]).is_err());
        let (sealed, _) = seal_entries(&entries(&ops)).unwrap();
        let halves = sponsored(&sealed);
        assert!(verify_vmops_aggregate(halves, &aggregate(&sealed)).is_err());
    }
    // A change of what an op signs, of its account, of the form of its signature, of the order, of the set
    for i in 0..halves.len() {
        let mut changed = halves.clone();
        changed[i].max_gas += 1;
        assert!(check(&changed, &s).is_err());
        let mut changed = halves.clone();
        changed[i].account = halves[(i + 1) % halves.len()].account;
        assert!(check(&changed, &s).is_err());
        let mut changed = halves.clone();
        changed[i].sig = match &halves[i].sig {
            SigScheme::HalfBip340(r) => SigScheme::HalfBip322(*r),
            SigScheme::HalfBip322(r) => SigScheme::HalfBip322All(*r),
            SigScheme::HalfBip322All(r) => SigScheme::HalfBip340(*r),
            full => panic!("not half: {full:?}"),
        };
        assert!(check(&changed, &s).is_err());
        let mut changed = halves.clone();
        changed.remove(i);
        assert!(check(&changed, &s).is_err());
    }
    let mut swapped = halves.clone();
    swapped.swap(0, 1);
    assert!(check(&swapped, &s).is_err());
    let mut other = s;
    other[31] ^= 1;
    assert!(check(&halves, &other).is_err());
}

#[test]
fn only_half_signatures_of_path_b_ops_are_aggregated() {
    let ops = ops();
    let (sealed, _) = seal_entries(&entries(&ops)).unwrap();
    let s = aggregate(&sealed);
    // A full signature among the halves
    let mut mixed: Vec<VmOp> = sponsored(&sealed).into_iter().cloned().collect();
    mixed[0] = ops[0].clone();
    assert_eq!(
        verify_vmops_aggregate(mixed.iter(), &s),
        Err(SigError::InvalidSignature)
    );
    // Sealed entries are not sealed again
    assert_eq!(seal_entries(&sealed), Err(SigError::InvalidSignature));
    // A BIP322 witness in any other form has no half and does not verify
    let mut odd = bip322(4, 0);
    let SigScheme::Bip322Simple(witness) = &mut odd.sig else {
        panic!()
    };
    witness.insert(0, 0);
    assert_eq!(verify_vmop(&odd), Err(SigError::InvalidSignature));
    assert_eq!(
        seal_entries(&entries(&[odd])),
        Err(SigError::InvalidSignature)
    );
    // A path C op is no part of an aggregate
    let mut native = bip340(5, 0);
    native.fee = FeeMode::BtcNative;
    assert_eq!(half_signed(&native), Err(SigError::FirstInputRequired));
}
