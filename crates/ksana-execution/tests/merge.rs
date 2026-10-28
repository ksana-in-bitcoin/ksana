//! Merged execution (D-063): a chain block first executes the merged blocks' entries in lenient mode (entries that
//! would make the block invalid are skipped without changing state, except a forced-inclusion entry naming no path-C
//! op, which makes the chain block invalid, D-100), then its own entries in strict mode; a merged block's sponsored fees
//! belong to its producer and are recorded at the merged block's own acceptance position (D-079).

mod common;

use common::*;
use ksana_execution::keys::LockedFee;
use ksana_execution::{check_entries_static, BlockError, MergedBlock, OpStatus};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Entry, DEVNET};

const GAS: u64 = 100_000;

fn merged_block(seed: &str, producer: &User, entries: Vec<Entry>) -> MergedBlock {
    MergedBlock {
        hash: sha256(seed.as_bytes()),
        producer: producer.account,
        entries,
        fast_path_paused: false,
    }
}

fn skipped(status: &OpStatus) -> bool {
    matches!(status, OpStatus::Skipped(_))
}

/// The same path-B op included by two parallel blocks: the earlier one executes, the later one is skipped; the
/// sponsored fee goes to the producer of the block that executed it.
#[test]
fn a_duplicate_op_in_two_merged_blocks_executes_once() {
    let (alice, bob) = (user("alice"), user("bob"));
    let (p1, p2) = (user("p1"), user("p2"));
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    let op = sponsored(&alice, 0, 2, GAS, transfer(bob.account, 5));
    let m1 = merged_block("m1", &p1, vec![op.clone()]);
    let m2 = merged_block("m2", &p2, vec![op.clone()]);
    let out = chain
        .chain_block(vec![m1.clone(), m2.clone()], vec![])
        .unwrap();

    assert_eq!(out.merged_receipts.len(), 2);
    assert_eq!(out.merged_receipts[0][0].status, OpStatus::Success);
    assert_eq!(out.merged_receipts[0][0].op_id, Some(op_id_of(&op)));
    assert!(skipped(&out.merged_receipts[1][0].status));
    assert_eq!(out.merged_receipts[1][0].op_id, Some(op_id_of(&op)));
    assert_eq!(out.merged_receipts[1][0].gas_used, 0);
    assert!(out.receipts.is_empty());

    assert_eq!(chain.ksa(&alice.account), 93);
    assert_eq!(chain.ksa(&bob.account), 5);
    assert_eq!(chain.nonce(&alice.account), 1);
    assert_eq!(
        chain.locked_fee(&m1.hash),
        Some(LockedFee {
            producer: p1.account,
            amount: 2,
        })
    );
    assert_eq!(chain.locked_fee(&m2.hash), None);
    // This block has no sponsored fee, so no record
    assert_eq!(chain.locked_fee(&chain.parent), None);
    assert_eq!(chain.position(&m1.hash), (1, 0));
    assert_eq!(chain.position(&m2.hash), (1, 1));
    assert_eq!(chain.position(&chain.parent), (1, 2));
}

/// Two different ops of the same account with the same nonce: only the earlier one executes; the skipped one is not
/// charged and does not change the nonce.
#[test]
fn a_conflicting_op_with_the_same_nonce_is_skipped() {
    let (alice, bob, carol) = (user("alice"), user("bob"), user("carol"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    let to_bob = sponsored(&alice, 0, 1, GAS, transfer(bob.account, 10));
    let to_carol = sponsored(&alice, 0, 3, GAS, transfer(carol.account, 20));
    let out = chain
        .chain_block(
            vec![
                merged_block("a", &p, vec![to_bob]),
                merged_block("b", &p, vec![to_carol]),
            ],
            vec![],
        )
        .unwrap();
    assert_eq!(out.merged_receipts[0][0].status, OpStatus::Success);
    assert!(skipped(&out.merged_receipts[1][0].status));
    assert_eq!(chain.ksa(&alice.account), 89);
    assert_eq!(chain.ksa(&bob.account), 10);
    assert_eq!(chain.ksa(&carol.account), 0);
    assert_eq!(chain.nonce(&alice.account), 1);
}

/// The various skips of lenient mode: an unlock entry (never executed when merged, D-100), insufficient balance,
/// non-consecutive nonce; entries after a skipped one execute as usual.
#[test]
fn entries_that_would_invalidate_a_block_are_skipped_when_merged() {
    let (alice, bob, dave) = (user("alice"), user("bob"), user("dave"));
    let p = user("p");
    let mut chain = Chain::new(vec![
        (alice.account, 100),
        (bob.account, 1),
        (dave.account, 50),
    ]);
    let entries = vec![
        // An unlock entry, here even naming a position not in the unlockable segment
        unlock(9, 0),
        // Balance insufficient for the fee
        sponsored(&bob, 0, 5, GAS, transfer(alice.account, 1)),
        // Non-consecutive nonce
        sponsored(&dave, 3, 1, GAS, transfer(alice.account, 1)),
        // Later entries execute as usual
        sponsored(&alice, 0, 1, GAS, transfer(bob.account, 4)),
    ];
    let out = chain
        .chain_block(vec![merged_block("m", &p, entries)], vec![])
        .unwrap();
    let statuses: Vec<_> = out.merged_receipts[0].iter().map(|r| &r.status).collect();
    assert!(statuses[..3].iter().all(|s| skipped(s)), "{statuses:?}");
    assert_eq!(*statuses[3], OpStatus::Success);
    // The receipt of an unlock entry has no op_id
    assert_eq!(out.merged_receipts[0][0].op_id, None);
    assert_eq!(chain.ksa(&alice.account), 95);
    assert_eq!(chain.ksa(&bob.account), 5);
    assert_eq!(chain.ksa(&dave.account), 50);
    assert_eq!(chain.nonce(&bob.account), 0);
    assert_eq!(chain.nonce(&dave.account), 0);
    assert_eq!(chain.nonce(&alice.account), 1);
}

/// Unlock entries of merged blocks are skipped (D-100), even when their position is unlockable: the fee stays locked
/// until a main-chain block's own unlock entry unlocks it.
#[test]
fn merged_unlock_entries_are_skipped() {
    let (alice, bob) = (user("alice"), user("bob"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    // Block 1 collects a sponsored fee, recorded at its acceptance position (1, 0)
    chain
        .block(vec![sponsored(&alice, 0, 6, GAS, transfer(bob.account, 1))])
        .unwrap();
    let block1 = chain.parent;
    chain.block(vec![]).unwrap();
    chain.btc.settle(block1);
    let producer = chain.producer;
    let out = chain
        .chain_block(
            vec![
                merged_block("u1", &p, vec![unlock(1, 0)]),
                merged_block("u2", &p, vec![unlock(1, 0)]),
            ],
            vec![],
        )
        .unwrap();
    assert!(skipped(&out.merged_receipts[0][0].status));
    assert!(skipped(&out.merged_receipts[1][0].status));
    assert_eq!(out.merged_receipts[0][0].op_id, None);
    assert_eq!(chain.ksa(&producer), 0);
    assert_eq!(chain.cursor(), None);
    assert!(chain.locked_fee(&block1).is_some());
    // The main-chain block's own unlock entry unlocks it and deletes the record (D-074)
    chain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 6);
    assert_eq!(chain.locked_fee(&block1), None);
}

/// Forced-inclusion entries behave as in strict mode when merged: the duplicate is recorded as failed because of the
/// nonce mismatch and changes no state.
#[test]
fn a_duplicate_forced_entry_fails_without_changing_state() {
    let (alice, bob) = (user("alice"), user("bob"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 10)]);
    let native = forced(&alice, 0, GAS, transfer(bob.account, 2));
    let out = chain
        .chain_block(
            vec![
                merged_block("f1", &p, vec![native.clone()]),
                merged_block("f2", &p, vec![native.clone()]),
            ],
            vec![],
        )
        .unwrap();
    assert_eq!(out.merged_receipts[0][0].status, OpStatus::Success);
    assert!(matches!(
        out.merged_receipts[1][0].status,
        OpStatus::Failed(_)
    ));
    assert_eq!(chain.ksa(&bob.account), 2);
    assert_eq!(chain.nonce(&alice.account), 1);
}

/// A forced-inclusion entry naming no native transaction of the anchor chain that carries its op stands for no path-C
/// op (D-100): among the block's own entries it makes the block invalid, and in a merged block it makes the block
/// merging it invalid, so that executing that block again in anchor-invalid mode (D-056), where the transaction is
/// looked up on the current chain, cannot find what the first execution did not. Nothing changes; the merged block's
/// other entries run once it is merged without that entry.
#[test]
fn a_forced_entry_naming_no_native_transaction_makes_the_merging_block_invalid() {
    let (alice, bob) = (user("alice"), user("bob"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 10)]);
    let ghost = forced(&alice, 0, GAS, transfer(bob.account, 2));
    let Entry::Forced { btc_txid, .. } = &ghost else {
        unreachable!()
    };
    chain.btc.absent.insert(*btc_txid);
    let next = sponsored(&alice, 0, 1, GAS, transfer(bob.account, 3));
    let err = chain.block(vec![ghost.clone()]).unwrap_err();
    assert!(
        matches!(&err, BlockError::Invalid(m) if m.contains("carries this op")),
        "{err:?}"
    );
    let err = chain
        .chain_block(
            vec![merged_block("g", &p, vec![ghost.clone(), next.clone()])],
            vec![],
        )
        .unwrap_err();
    assert!(
        matches!(&err, BlockError::Invalid(m) if m.contains("merged block") && m.contains("carries this op")),
        "{err:?}"
    );
    // ... wherever it comes in the merge set
    let err = chain
        .chain_block(
            vec![
                merged_block("n", &p, vec![next.clone()]),
                merged_block("g", &p, vec![ghost]),
            ],
            vec![],
        )
        .unwrap_err();
    assert!(matches!(&err, BlockError::Invalid(_)), "{err:?}");
    assert_eq!(chain.ksa(&alice.account), 10);
    assert_eq!(chain.nonce(&alice.account), 0);
    let out = chain
        .chain_block(vec![merged_block("n", &p, vec![next])], vec![])
        .unwrap();
    assert_eq!(out.merged_receipts[0][0].status, OpStatus::Success);
    assert_eq!(chain.ksa(&alice.account), 6);
    assert_eq!(chain.ksa(&bob.account), 3);
    assert_eq!(chain.nonce(&alice.account), 1);
}

/// A merged block's sponsored fees are recorded at its own acceptance position, this block's at its own position (after
/// the merged blocks, D-079). This block's entries execute in strict mode after the merged blocks: a conflict with a
/// merged block makes the block invalid.
#[test]
fn own_entries_run_strictly_after_the_merged_blocks() {
    let (alice, bob) = (user("alice"), user("bob"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    let first = sponsored(&alice, 0, 2, GAS, transfer(bob.account, 1));
    let second = sponsored(&alice, 1, 3, GAS, transfer(bob.account, 1));
    let m = merged_block("m", &p, vec![first.clone()]);

    // This block includes an op already in a merged block: the nonce is used up, the block is invalid
    let mut conflicting = Chain::new(vec![(alice.account, 100)]);
    assert!(matches!(
        conflicting.chain_block(vec![m.clone()], vec![first.clone()]),
        Err(BlockError::Invalid(_))
    ));

    let out = chain.chain_block(vec![m.clone()], vec![second]).unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert_eq!(chain.nonce(&alice.account), 2);
    assert_eq!(chain.locked_fee(&m.hash).unwrap().amount, 2);
    assert_eq!(chain.locked_fee(&m.hash).unwrap().producer, p.account);
    let own = chain.locked_fee(&chain.parent).unwrap();
    assert_eq!((own.producer, own.amount), (chain.producer, 3));
    assert_eq!(chain.position(&chain.parent), (1, 1));
}

/// Entries of merged blocks do not count toward this block's total gas (each block's limit is checked by its own
/// validity rules).
#[test]
fn merged_entries_do_not_count_toward_the_block_gas_limit() {
    let (alice, bob) = (user("alice"), user("bob"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    let limit = DEVNET.block_gas_limit;
    let big = sponsored(&alice, 0, 1, limit, transfer(bob.account, 1));
    let own = sponsored(&alice, 1, 1, limit, transfer(bob.account, 1));
    let out = chain
        .chain_block(vec![merged_block("big", &p, vec![big])], vec![own])
        .unwrap();
    assert_eq!(out.merged_receipts[0][0].status, OpStatus::Success);
    assert_eq!(out.receipts[0].status, OpStatus::Success);
}

/// Settlement backlog limit (D-046): the merged block's fast-path entries are skipped; forced-inclusion, arrears and
/// unlock entries proceed as usual.
#[test]
fn a_paused_merged_block_skips_its_fast_path_entries() {
    let (alice, bob) = (user("alice"), user("bob"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    let mut m = merged_block(
        "paused",
        &p,
        vec![
            forced(&alice, 0, GAS, transfer(bob.account, 1)),
            sponsored(&alice, 1, 2, GAS, transfer(bob.account, 3)),
        ],
    );
    m.fast_path_paused = true;
    let out = chain.chain_block(vec![m], vec![]).unwrap();
    assert_eq!(out.merged_receipts[0][0].status, OpStatus::Success);
    assert!(skipped(&out.merged_receipts[0][1].status));
    assert_eq!(chain.ksa(&bob.account), 1);
    assert_eq!(chain.nonce(&alice.account), 1);
}

/// With an empty merge set it is identical to `execute_block`.
#[test]
fn an_empty_mergeset_matches_execute_block() {
    let (alice, bob) = (user("alice"), user("bob"));
    let entries = vec![
        forced(&alice, 0, GAS, transfer(bob.account, 1)),
        sponsored(&alice, 1, 2, GAS, transfer(bob.account, 3)),
    ];
    let mut a = Chain::new(vec![(alice.account, 100)]);
    let mut b = Chain::new(vec![(alice.account, 100)]);
    let out_a = a.block(entries.clone()).unwrap();
    let out_b = b.chain_block(vec![], entries).unwrap();
    assert_eq!(out_a, out_b);
    assert!(out_a.merged_receipts.is_empty());
}

/// Entry rules that do not depend on state (merged blocks must satisfy them too, D-063).
#[test]
fn static_entry_rules() {
    let (alice, bob) = (user("alice"), user("bob"));
    let ok = vec![
        forced(&alice, 0, GAS, transfer(bob.account, 1)),
        unlock(1, 0),
        sponsored(&alice, 1, 2, GAS, transfer(bob.account, 3)),
    ];
    assert_eq!(check_entries_static(&DEVNET, &DEVNET.rules(0), &ok), Ok(()));
    // Order
    let reversed: Vec<Entry> = ok.iter().rev().cloned().collect();
    assert!(check_entries_static(&DEVNET, &DEVNET.rules(0), &reversed).is_err());
    // Total gas
    let over = vec![
        sponsored(
            &alice,
            0,
            1,
            DEVNET.block_gas_limit,
            transfer(bob.account, 1),
        ),
        sponsored(&alice, 1, 1, 1, transfer(bob.account, 1)),
    ];
    assert!(check_entries_static(&DEVNET, &DEVNET.rules(0), &over).is_err());
    // Invalid signature
    let mut bad = sponsored(&alice, 0, 1, GAS, transfer(bob.account, 1));
    if let Entry::Sponsored { op } = &mut bad {
        op.nonce = 9;
    }
    assert!(check_entries_static(&DEVNET, &DEVNET.rules(0), &[bad.clone()]).is_err());
    // Fee mode does not match the entry type
    let wrong_fee = Entry::Sponsored {
        op: native_op(&alice, 0, [0x51; 32], GAS, transfer(bob.account, 1)),
    };
    assert!(check_entries_static(&DEVNET, &DEVNET.rules(0), &[wrong_fee]).is_err());
    // A signature problem on a forced-inclusion entry only fails the op
    let mut bad_native = forced(&alice, 0, GAS, transfer(bob.account, 1));
    if let Entry::Forced { op, .. } = &mut bad_native {
        op.nonce = 9;
    }
    assert_eq!(
        check_entries_static(&DEVNET, &DEVNET.rules(0), &[bad_native]),
        Ok(())
    );
    // Deploy ops go through the same checks
    let deploy = sponsored(
        &alice,
        0,
        1,
        GAS,
        Action::Deploy {
            code: vec![],
            init_args: vec![],
            funds: Vec::new(),
        },
    );
    assert_eq!(
        check_entries_static(&DEVNET, &DEVNET.rules(0), &[deploy]),
        Ok(())
    );
}
