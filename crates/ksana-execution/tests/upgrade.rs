//! Network upgrades (D-076): blocks are checked and executed under the rules at their own DAA score — the block gas
//! limit, the native op gas limit and the VmOp version range changed by an upgrade differ before and after the
//! activation point. With an empty upgrade table the rules are the values in the params (the determinism test's fixed
//! state roots are unchanged).

mod common;

use common::*;
use ksana_crypto::sign_vmop;
use ksana_execution::{check_entries_static, BlockError, OpStatus};
use ksana_primitives::{Entry, FeeMode, Params, Upgrade};

/// From DAA score 3: protocol version 3, the block and native op gas limits double, VmOp versions 2-3 are accepted.
const UPGRADES: &[Upgrade] = &[Upgrade {
    block_gas_limit: Some(2 * NO_REWARD.block_gas_limit),
    native_max_gas: Some(2 * NO_REWARD.native_max_gas),
    max_vmop_version: Some(3),
    ..Upgrade::new(3, 3)
}];

fn params() -> Params {
    Params {
        upgrades: UPGRADES,
        ..NO_REWARD
    }
}

/// A path-B transfer with VmOp version `version` (1 KSA sponsored fee).
fn versioned(u: &User, nonce: u64, version: u8, max_gas: u64, to: [u8; 32]) -> Entry {
    let mut op = op(
        u,
        nonce,
        FeeMode::Token { amount: 1 },
        max_gas,
        transfer(to, 1),
    );
    op.version = version;
    op.sig = sign_vmop(&op, &u.kp, &[0u8; 32]).unwrap();
    Entry::Sponsored { op }
}

#[test]
fn static_rules_follow_the_block_version() {
    let p = params();
    let (old, new) = (p.rules(2), p.rules(3));
    assert_eq!((old.version, new.version), (2, 3));
    let (alice, bob) = (user("upgrade/alice"), user("upgrade/bob"));
    // Two ops totalling 1.2 times the old block gas limit: invalid under the old rules, valid under the new
    let part = NO_REWARD.block_gas_limit * 3 / 5;
    let big = vec![
        versioned(&alice, 0, 2, part, bob.account),
        versioned(&alice, 1, 2, part, bob.account),
    ];
    let err = check_entries_static(&p, &old, &big).unwrap_err();
    assert!(err.contains("BLOCK_GAS_LIMIT"), "{err}");
    assert_eq!(check_entries_static(&p, &new, &big), Ok(()));
    // VmOp version 3 is accepted only under the new rules, version 4 under neither
    let v3 = vec![versioned(&alice, 0, 3, 100_000, bob.account)];
    let err = check_entries_static(&p, &old, &v3).unwrap_err();
    assert!(err.contains("unsupported VmOp version 3"), "{err}");
    assert_eq!(check_entries_static(&p, &new, &v3), Ok(()));
    let v4 = vec![versioned(&alice, 0, 4, 100_000, bob.account)];
    assert!(check_entries_static(&p, &new, &v4).is_err());
}

#[test]
fn blocks_execute_under_the_rules_of_their_daa_score() {
    let (alice, bob) = (user("upgrade/alice2"), user("upgrade/bob2"));
    let mut chain = Chain::with_params(params(), vec![(alice.account, 1_000)]);
    // Blocks 1 and 2 (DAA scores 1, 2) follow the old rules: a version-3 op makes the block invalid
    assert_eq!(chain.env().rules.version, 2);
    let err = chain
        .block(vec![versioned(&alice, 0, 3, 100_000, bob.account)])
        .unwrap_err();
    assert!(
        matches!(&err, BlockError::Invalid(m) if m.contains("unsupported VmOp version 3")),
        "{err:?}"
    );
    // A native op's max_gas above the old limit: only fails the op (consuming nonce 0, revision of D-018)
    let native = forced(
        &alice,
        0,
        NO_REWARD.native_max_gas + 1,
        transfer(bob.account, 1),
    );
    let out = chain.block(vec![native]).unwrap();
    assert!(
        matches!(&out.receipts[0].status, OpStatus::Failed(m) if m.contains("NATIVE_MAX_GAS")),
        "{:?}",
        out.receipts[0].status
    );
    chain
        .block(vec![versioned(&alice, 1, 2, 100_000, bob.account)])
        .unwrap();

    // From block 3 the new rules apply: a version-3 op executes as usual, and so does a native op with the same max_gas
    assert_eq!(chain.env().rules.version, 3);
    let out = chain
        .block(vec![versioned(&alice, 2, 3, 100_000, bob.account)])
        .unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    let native = forced(
        &alice,
        3,
        NO_REWARD.native_max_gas + 1,
        transfer(bob.account, 1),
    );
    let out = chain.block(vec![native]).unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert_eq!(chain.ksa(&bob.account), 3);
}
