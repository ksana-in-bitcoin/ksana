//! The rule on the signatures of a block's path B entries (SPEC §4.5, D-154): a block carries half of each signature
//! and one aggregate entry after its path B entries, and it is valid only when the aggregate verifies. The producer
//! builds from full signatures and gets the same block.

mod common;

use std::sync::Arc;

use common::*;
use ksana_crypto::seal_entries;
use ksana_execution::{check_aggregate, check_entries_static, BlockError, MergedBlock, OpStatus};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Entry, SigScheme, DEVNET};

const GAS: u64 = 100_000;

fn invalid(chain: &Chain, entries: &[Entry], needle: &str) {
    let statically = check_entries_static(&DEVNET, &DEVNET.rules(0), entries);
    let executed = chain.exec.execute_block(
        Arc::new(chain.state.clone()),
        Arc::new(chain.btc.clone()),
        &chain.env(),
        entries,
    );
    let reason = statically.expect_err("the static rules refuse the entries");
    assert!(reason.contains(needle), "{reason}");
    match executed {
        Err(BlockError::Invalid(reason)) => assert!(reason.contains(needle), "{reason}"),
        other => panic!("the block is not invalid: {other:?}"),
    }
}

fn set_up() -> (Chain, Vec<Entry>) {
    let (alice, bob) = (user("agg/alice"), user("agg/bob"));
    let chain = Chain::new(vec![(alice.account, 1_000), (bob.account, 1_000)]);
    let entries = vec![
        forced(&bob, 0, GAS, transfer(alice.account, 1)),
        sponsored(&alice, 0, 2, GAS, transfer(bob.account, 3)),
        sponsored(&bob, 1, 2, GAS, transfer(alice.account, 5)),
        sponsored(&alice, 1, 2, GAS, transfer(bob.account, 7)),
    ];
    (chain, entries)
}

#[test]
fn a_block_is_valid_by_its_aggregate_and_the_producer_builds_the_same_block() {
    let (chain, entries) = set_up();
    let (sealed, seconds) = seal_entries(&entries).unwrap();
    assert_eq!(seconds.len(), 3);
    assert_eq!(sealed.len(), entries.len() + 1);
    check_entries_static(&DEVNET, &DEVNET.rules(0), &sealed).unwrap();
    let env = chain.env();
    let validated = chain
        .exec
        .execute_block(
            Arc::new(chain.state.clone()),
            Arc::new(chain.btc.clone()),
            &env,
            &sealed,
        )
        .unwrap();
    assert_eq!(validated.receipts.len(), sealed.len());
    assert!(validated
        .receipts
        .iter()
        .all(|r| r.status == OpStatus::Success));
    // The receipt of an op names the same ID before the block and in it, and the aggregate entry has none
    for (entry, receipt) in entries.iter().zip(&validated.receipts) {
        assert_eq!(receipt.op_id, Some(op_id_of(entry)));
    }
    assert_eq!(validated.receipts[4].op_id, None);
    assert_eq!(validated.receipts[4].gas_used, 0);

    // The producer pushes the ops with their full signatures, then the aggregate
    let (built, ()) = chain
        .exec
        .build_block(
            Arc::new(chain.state.clone()),
            Arc::new(chain.btc.clone()),
            &env,
            |builder| {
                for entry in &entries {
                    builder.push(entry)?;
                }
                builder.push(&sealed[4])?;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(built, validated);
}

#[test]
fn the_aggregate_entry_is_the_last_and_only_with_path_b_entries() {
    let (chain, entries) = set_up();
    let (sealed, _) = seal_entries(&entries).unwrap();
    let aggregate = sealed[4].clone();

    // Full signatures and no aggregate: what a block was before D-154
    invalid(&chain, &entries, "exactly one aggregate entry");
    // Half signatures without the aggregate
    invalid(&chain, &sealed[..4], "exactly one aggregate entry");
    // Two aggregate entries, or one that is not the last
    let mut twice = sealed.clone();
    twice.push(aggregate.clone());
    assert!(check_entries_static(&DEVNET, &DEVNET.rules(0), &twice).is_err());
    assert!(check_aggregate(&twice).is_err());
    let mut early = sealed.clone();
    early.swap(3, 4);
    assert!(check_entries_static(&DEVNET, &DEVNET.rules(0), &early).is_err());
    assert!(check_aggregate(&early).is_err());
    // An aggregate entry without path B entries
    invalid(
        &chain,
        &[sealed[0].clone(), aggregate.clone()],
        "without path B entries",
    );
    invalid(&chain, &[aggregate], "without path B entries");
    // A full signature among the halves
    let mut mixed = sealed.clone();
    mixed[2] = entries[2].clone();
    invalid(&chain, &mixed, "carries a whole signature");
    // A block without path B entries has no aggregate entry
    check_aggregate(&sealed[..1]).unwrap();
    check_aggregate(&[]).unwrap();
}

#[test]
fn a_block_whose_aggregate_does_not_verify_is_invalid() {
    let (chain, entries) = set_up();
    let (sealed, _) = seal_entries(&entries).unwrap();
    let needle = "the block's aggregate";

    // Another scalar
    let mut other = sealed.clone();
    let Some(Entry::Aggregate { s }) = other.last_mut() else {
        panic!("no aggregate entry")
    };
    s[31] ^= 1;
    invalid(&chain, &other, needle);
    // A scalar that is not below the group's order
    let mut large = sealed.clone();
    *large.last_mut().unwrap() = Entry::Aggregate { s: [0xff; 32] };
    invalid(&chain, &large, needle);
    // An op changed, dropped, or moved after the aggregate was made
    let mut changed = sealed.clone();
    if let Entry::Sponsored { op } = &mut changed[2] {
        op.max_gas += 1;
    }
    invalid(&chain, &changed, needle);
    let mut dropped = sealed.clone();
    dropped.remove(2);
    invalid(&chain, &dropped, needle);
    let mut moved = sealed.clone();
    moved.swap(1, 2);
    invalid(&chain, &moved, needle);
    // A first half that is no point of the curve
    let mut off_curve = sealed.clone();
    if let Entry::Sponsored { op } = &mut off_curve[1] {
        op.sig = SigScheme::HalfBip340([0xff; 32]);
    }
    invalid(&chain, &off_curve, needle);
    // The form of a signature is part of what the aggregate covers
    let mut form = sealed.clone();
    if let Entry::Sponsored { op } = &mut form[1] {
        let SigScheme::HalfBip340(r) = op.sig else {
            panic!("not half a BIP340 signature")
        };
        op.sig = SigScheme::HalfBip322(r);
    }
    invalid(&chain, &form, needle);

    // One signature that does not verify among those that do: the producer refuses the op, and a block sealed with
    // it is invalid
    let mut forged = entries.clone();
    if let Entry::Sponsored { op } = &mut forged[2] {
        op.nonce = 9;
    }
    let (sealed_forged, _) = seal_entries(&forged).unwrap();
    invalid(&chain, &sealed_forged, needle);
    let refused = chain.exec.build_block(
        Arc::new(chain.state.clone()),
        Arc::new(chain.btc.clone()),
        &chain.env(),
        |builder| {
            builder.push(&forged[1])?;
            builder.push(&forged[2])?;
            Ok(())
        },
    );
    assert!(matches!(refused, Err(BlockError::Invalid(m)) if m.contains("invalid signature")));
}

#[test]
fn the_producer_takes_full_signatures_only_and_ends_with_the_aggregate() {
    let (chain, entries) = set_up();
    let (sealed, _) = seal_entries(&entries).unwrap();
    let build = |pushed: &[Entry]| {
        chain
            .exec
            .build_block(
                Arc::new(chain.state.clone()),
                Arc::new(chain.btc.clone()),
                &chain.env(),
                |builder| {
                    for entry in pushed {
                        builder.push(entry)?;
                    }
                    Ok(())
                },
            )
            .map(|(outcome, ())| outcome)
    };
    let reason = |result: Result<_, BlockError>| match result {
        Err(BlockError::Invalid(reason)) => reason,
        other => panic!(
            "not invalid: {:?}",
            other.map(|_: ksana_execution::BlockOutcome| ())
        ),
    };
    // Half a signature proves nothing to the producer
    assert!(reason(build(&sealed[..2])).contains("outside a block's aggregate"));
    // Path B entries without the aggregate after them
    assert!(reason(build(&entries)).contains("ends with its aggregate entry"));
    // Nothing follows the aggregate, and it follows path B entries
    let mut after = entries.clone();
    after.push(sealed[4].clone());
    after.push(sealed[4].clone());
    assert!(reason(build(&after)).contains("the last entry"));
    assert!(reason(build(&[sealed[0].clone(), sealed[4].clone()])).contains("the last entry"));
    // The scalar the producer pushes is not checked while building: the block it gives is checked as any other
    let mut wrong = entries.clone();
    wrong.push(Entry::Aggregate { s: [7; 32] });
    build(&wrong).unwrap();
}

/// A merged block passed the static rules, its aggregate among them; its path B entries then run with half
/// signatures, and one carrying a full signature (a block that did not pass them) is skipped.
#[test]
fn a_merged_blocks_path_b_entries_run_on_its_aggregate() {
    let (mut chain, entries) = set_up();
    let producer = user("agg/producer");
    let (sealed, _) = seal_entries(&entries).unwrap();
    let block = |seed: &str, entries: Vec<Entry>| MergedBlock {
        hash: sha256(seed.as_bytes()),
        producer: producer.account,
        entries,
        fast_path_paused: false,
    };
    let out = chain
        .exec
        .execute_chain_block(
            Arc::new(chain.state.clone()),
            Arc::new(chain.btc.clone()),
            &chain.env(),
            &[
                block("sealed", sealed.clone()),
                block(
                    "unsealed",
                    vec![sponsored(&user("agg/bob"), 2, 2, GAS, transfer([9; 32], 1))],
                ),
            ],
            &[],
        )
        .unwrap();
    assert_eq!(out.merged_receipts[0].len(), 5);
    assert!(out.merged_receipts[0]
        .iter()
        .all(|r| r.status == OpStatus::Success));
    assert!(
        matches!(&out.merged_receipts[1][0].status, OpStatus::Skipped(m) if m.contains("carries a whole signature"))
    );
    chain.state.apply(&out.writes);
    assert_eq!(chain.nonce(&user("agg/alice").account), 2);
    assert_eq!(chain.nonce(&user("agg/bob").account), 2);
}
