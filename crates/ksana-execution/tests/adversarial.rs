//! Adversarial tests: how the execution layer handles cross-network replay and copies of path-C ops (D-086, §7.4).
//!
//! - An op signed for another network (different `chain_id`): as a path-B entry it makes the block invalid (the
//!   producer cannot stuff it into a block), as a forced-inclusion entry it only fails the op (§7.4: static problems of
//!   forced-inclusion entries do not affect block validity); neither changes state or consumes the nonce.
//! - A signed path-C op copied into another BTC transaction: the copy's signature is bound to the original's first
//!   input, so signature verification fails in its own transaction and no nonce is consumed; the original executes as
//!   usual whether it comes before or after the copy; an op that already executed and is copied into a new transaction
//!   fails just the same.

mod common;

use common::*;
use ksana_crypto::{sign_native_vmop, sign_vmop};
use ksana_execution::{BlockError, OpStatus};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Entry, FeeMode, SIGNET};

const GAS: u64 = 100_000;

fn failed_with(status: &OpStatus, needle: &str) -> bool {
    matches!(status, OpStatus::Failed(m) if m.contains(needle))
}

#[test]
fn an_op_signed_for_another_network_invalidates_a_block_as_path_b_and_only_fails_as_forced() {
    let (alice, bob) = (user("adv/alice"), user("adv/bob"));
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    assert_ne!(SIGNET.chain_id, chain.exec.params().chain_id);

    // Path B: the signature is valid (signed with the other network's chain ID), but the block is invalid
    let mut op = unsigned(
        &alice,
        0,
        FeeMode::Token { amount: 3 },
        GAS,
        transfer(bob.account, 5),
    );
    op.chain_id = SIGNET.chain_id;
    op.sig = sign_vmop(&op, &alice.kp, &[0; 32]).unwrap();
    assert_eq!(ksana_crypto::verify_vmop(&op), Ok(()));
    let err = chain.block(vec![Entry::Sponsored { op }]).unwrap_err();
    assert!(
        matches!(&err, BlockError::Invalid(m) if m.contains("chain_id")),
        "{err:?}"
    );
    assert_eq!(chain.height, 0);
    assert_eq!(chain.nonce(&alice.account), 0);
    assert_eq!(chain.ksa(&bob.account), 0);

    // Forced inclusion: only the op fails
    let btc_txid = sha256(b"adv/other-network");
    let mut native = unsigned(&alice, 0, FeeMode::BtcNative, GAS, transfer(bob.account, 5));
    native.chain_id = SIGNET.chain_id;
    native.sig =
        sign_native_vmop(&native, &test_first_input(&btc_txid), &alice.kp, &[0; 32]).unwrap();
    let out = chain
        .block(vec![Entry::Forced {
            btc_txid,
            op: native,
        }])
        .unwrap();
    assert!(
        failed_with(&out.receipts[0].status, "chain_id"),
        "{:?}",
        out.receipts[0]
    );
    assert_eq!(chain.height, 1);
    assert_eq!(chain.nonce(&alice.account), 0);
    assert_eq!(chain.ksa(&bob.account), 0);
    assert_eq!(chain.ksa(&alice.account), 100);
}

#[test]
fn a_forced_entry_carrying_a_copied_op_fails_without_consuming_the_nonce() {
    let (alice, bob) = (user("adv/alice2"), user("adv/bob2"));
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    let original = sha256(b"adv/original");
    let copy = sha256(b"adv/copy");
    // The op is bound to the original's first input; the copy's transaction has its own first input (two transactions
    // cannot spend the same output)
    let op = native_op(&alice, 0, original, GAS, transfer(bob.account, 9));
    assert_ne!(test_first_input(&original), test_first_input(&copy));

    // Copy first, original after (BTC order): the copy fails without consuming the nonce, the original succeeds
    let out = chain
        .block(vec![
            Entry::Forced {
                btc_txid: copy,
                op: op.clone(),
            },
            Entry::Forced {
                btc_txid: original,
                op: op.clone(),
            },
        ])
        .unwrap();
    assert!(
        failed_with(&out.receipts[0].status, "signature"),
        "{:?}",
        out.receipts[0]
    );
    assert_eq!(out.receipts[1].status, OpStatus::Success);
    assert_eq!(chain.nonce(&alice.account), 1);
    assert_eq!(chain.ksa(&bob.account), 9);

    // Original first, copy after
    let original2 = sha256(b"adv/original2");
    let copy2 = sha256(b"adv/copy2");
    let op2 = native_op(&alice, 1, original2, GAS, transfer(bob.account, 1));
    let out = chain
        .block(vec![
            Entry::Forced {
                btc_txid: original2,
                op: op2.clone(),
            },
            Entry::Forced {
                btc_txid: copy2,
                op: op2.clone(),
            },
        ])
        .unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert!(
        failed_with(&out.receipts[1].status, "signature"),
        "{:?}",
        out.receipts[1]
    );
    assert_eq!(chain.nonce(&alice.account), 2);
    assert_eq!(chain.ksa(&bob.account), 10);

    // An op that already executed is copied into a new transaction in a later block: fails just the same
    let copy3 = sha256(b"adv/copy3");
    let out = chain
        .block(vec![Entry::Forced {
            btc_txid: copy3,
            op: op2,
        }])
        .unwrap();
    assert!(matches!(out.receipts[0].status, OpStatus::Failed(_)));
    assert_eq!(chain.nonce(&alice.account), 2);
    assert_eq!(chain.ksa(&bob.account), 10);
    assert_eq!(chain.ksa(&alice.account), 90);
}
