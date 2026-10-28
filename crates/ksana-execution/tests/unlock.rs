//! Unlock entries (`Unlock { height, index }`, SPEC §6.2, §8.5; D-035, D-038, D-074, D-079).
//!
//! Sponsored fees arise through the real flow: block N contains path-B entries and the fees are recorded at
//! `0x08 ‖ N ‖ acceptance ordinal`. Unlocking is cumulative in acceptance order: from after the unlock cursor up to and
//! including the position named by the entry, the records are deleted right away and the cursor advances; the target
//! must be in the unlockable segment given by the node (at most `MAX_UNLOCK_BATCH` positions) and must have entered the
//! settled closure; at most one unlock entry per block. The chain here has no block rewards ([`NO_REWARD`]); block
//! rewards are covered in rewards.rs.

mod common;

use std::sync::Arc;

use common::*;
use ksana_execution::keys::{self, LockedFee, UnlockCursor};
use ksana_execution::{check_entries_static, BlockError, MergedBlock, OpStatus, Receipt};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Entry, Params};

const GAS: u64 = 5_000_000;
/// The producer's initial KSA.
const PRODUCER_KSA: u128 = 500;

struct Setup {
    chain: Chain,
    a: User,
    b: User,
    /// Hash of block 1: block 1's sponsored fees are recorded at its acceptance position (1, 0).
    block1: [u8; 32],
}

/// Block 1: a submits two ops with 7 + 5 KSA in sponsored fees.
fn setup() -> Setup {
    let (a, b) = (user("a"), user("b"));
    let producer = user("producer").account;
    let mut chain = Chain::new(vec![(a.account, 1_000), (producer, PRODUCER_KSA)]);
    chain
        .block(vec![
            sponsored(&a, 0, 7, GAS, transfer(b.account, 1)),
            sponsored(&a, 1, 5, GAS, transfer(b.account, 1)),
        ])
        .unwrap();
    let block1 = chain.parent;
    Setup {
        chain,
        a,
        b,
        block1,
    }
}

fn invalid_reason(result: Result<impl std::fmt::Debug, BlockError>) -> String {
    match result {
        Err(BlockError::Invalid(reason)) => reason,
        other => panic!("the block should be invalid: {other:?}"),
    }
}

/// The block is invalid, the reason contains `needle`, and the chain's state is unchanged.
fn assert_invalid(chain: &mut Chain, entries: Vec<Entry>, needle: &str) {
    let before = chain.state.0.clone();
    let reason = invalid_reason(chain.block(entries));
    assert!(
        reason.contains(needle),
        "the invalidity reason should contain `{needle}`: {reason}"
    );
    assert_eq!(
        chain.state.0, before,
        "an invalid block does not change state"
    );
}

#[test]
fn fees_are_recorded_at_the_accepted_position_when_received() {
    let Setup {
        chain, a, block1, ..
    } = setup();
    // The block that receives the sponsored fee writes it at its acceptance position right away (D-079); there is no
    // pending record any more
    assert_eq!(chain.position(&block1), (1, 0));
    assert_eq!(
        chain.get::<LockedFee>(&keys::locked_fee(1, 0)),
        Some(LockedFee {
            producer: chain.producer,
            amount: 12
        })
    );
    assert_eq!(chain.ksa(&a.account), 1_000 - 12 - 2);
    assert_eq!(chain.cursor(), None, "nothing unlocked yet");
}

#[test]
fn unlock_moves_the_locked_fee_to_the_producer_available_balance() {
    let Setup {
        mut chain, block1, ..
    } = setup();
    let producer = chain.producer;
    chain.btc.settle(block1);
    let out = chain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(
        out.receipts,
        vec![Receipt {
            op_id: None,
            status: OpStatus::Success,
            gas_used: 0,
            output: vec![],
            events: vec![],
            asset_changes: vec![],
        }]
    );
    assert_eq!(chain.ksa(&producer), PRODUCER_KSA + 12);
    assert_eq!(
        chain.locked_fee(&block1),
        None,
        "unlocking deletes the record"
    );
    assert_eq!(chain.cursor(), Some((1, 0)));
    assert_eq!(
        chain.get::<UnlockCursor>(&keys::unlock_cursor()),
        Some(UnlockCursor {
            height: 1,
            index: 0
        })
    );

    // The same position can be unlocked only once: unlocking it again in a later block is invalid
    assert_invalid(&mut chain, vec![unlock(1, 0)], "already unlocked");
    assert_eq!(chain.ksa(&producer), PRODUCER_KSA + 12);
}

#[test]
fn unlock_in_a_later_block_and_twice_in_one_block() {
    let Setup {
        mut chain,
        a,
        b,
        block1,
    } = setup();
    let producer = chain.producer;
    // Block 2: the settlement is not yet confirmed; block 2 itself also collected a sponsored fee
    chain
        .block(vec![sponsored(&a, 2, 3, GAS, transfer(b.account, 1))])
        .unwrap();
    let block2 = chain.parent;
    assert_eq!(chain.locked_fee(&block1).unwrap().amount, 12);
    assert_invalid(&mut chain, vec![unlock(1, 0)], "settled closure");
    assert_invalid(&mut chain, vec![unlock(2, 0)], "settled closure");

    // After block 1 enters the settled closure: two unlock entries in one block are invalid (at most one per block,
    // D-074), the whole block is rejected
    chain.btc.settle(block1);
    assert_invalid(&mut chain, vec![unlock(1, 0), unlock(1, 0)], "at most one");
    // Block 2 enters the settled closure: one entry cumulatively unlocks up to block 2, block 1 before it is unlocked
    // too, then comes a path-B entry
    chain.btc.settle(block2);
    let out = chain
        .block(vec![
            unlock(2, 0),
            sponsored(&a, 3, 4, GAS, transfer(b.account, 1)),
        ])
        .unwrap();
    assert_eq!(out.receipts.len(), 2);
    assert!(out.receipts.iter().all(|r| r.status == OpStatus::Success));
    assert_eq!(chain.ksa(&producer), PRODUCER_KSA + 12 + 3);
    assert_eq!(chain.locked_fee(&block1), None);
    assert_eq!(chain.locked_fee(&block2), None);
    // This block's (height 3) 4 KSA are recorded at its own position, unaffected by the unlock
    assert_eq!(chain.locked_fee(&chain.parent).unwrap().amount, 4);
    assert_eq!(chain.cursor(), Some((2, 0)));
}

/// Blocks 1, 2 and 3 each collect a sponsored fee, block 4 is empty: the acceptance order is (1,0), (2,0), (3,0),
/// (4,0). Unlocking up to block 2 unlocks block 1 too and leaves block 3 alone; block 1 cannot be unlocked again.
#[test]
fn unlock_is_cumulative_in_accepted_order() {
    let Setup {
        mut chain,
        a,
        b,
        block1,
        ..
    } = setup();
    let producer = chain.producer;
    chain
        .block(vec![sponsored(&a, 2, 3, GAS, transfer(b.account, 1))])
        .unwrap();
    let block2 = chain.parent;
    chain
        .block(vec![sponsored(&a, 3, 4, GAS, transfer(b.account, 1))])
        .unwrap();
    let block3 = chain.parent;
    chain.block(vec![]).unwrap();
    let positions: Vec<_> = chain.unlockable().iter().map(|i| i.position()).collect();
    assert_eq!(positions, vec![(1, 0), (2, 0), (3, 0), (4, 0)]);

    // Only block 2 is marked settled (the settled closure is monotonic: under a real view block 1 must be settled too);
    // only the position named by the entry is checked
    chain.btc.settle(block2);
    chain.block(vec![unlock(2, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), PRODUCER_KSA + 12 + 3);
    assert_eq!(chain.locked_fee(&block3).unwrap().amount, 4);
    let positions: Vec<_> = chain.unlockable().iter().map(|i| i.position()).collect();
    assert_eq!(positions, vec![(3, 0), (4, 0), (5, 0)]);

    // Block 1 was unlocked along with block 2
    chain.btc.settle(block1);
    assert_invalid(&mut chain, vec![unlock(1, 0)], "already unlocked");
    // Block 3 can only be unlocked after it enters the settled closure
    assert_invalid(&mut chain, vec![unlock(3, 0)], "settled closure");
    chain.btc.settle(block3);
    chain.block(vec![unlock(3, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), PRODUCER_KSA + 12 + 3 + 4);
    // Positions without sponsored fees can be targets just the same: the cursor moves past them
    let block4 = sha256(&4u64.to_le_bytes());
    chain.btc.settle(block4);
    chain.block(vec![unlock(4, 0)]).unwrap();
    assert_eq!(chain.cursor(), Some((4, 0)));
    assert_eq!(chain.ksa(&producer), PRODUCER_KSA + 12 + 3 + 4);
}

/// The unlockable segment has at most `MAX_UNLOCK_BATCH` positions: an entry with a farther target is invalid; unlock
/// earlier positions first.
#[test]
fn unlock_batch_is_bounded() {
    let (a, b) = (user("a"), user("b"));
    let params = Params {
        max_unlock_batch: 2,
        ..NO_REWARD
    };
    let mut chain = Chain::with_params(params, vec![(a.account, 1_000)]);
    let producer = chain.producer;
    let mut blocks = Vec::new();
    for nonce in 0..3 {
        chain
            .block(vec![sponsored(&a, nonce, 1, GAS, transfer(b.account, 1))])
            .unwrap();
        blocks.push(chain.parent);
    }
    chain.block(vec![]).unwrap();
    for block in &blocks {
        chain.btc.settle(*block);
    }
    assert_eq!(chain.unlockable().len(), 2);
    assert_invalid(&mut chain, vec![unlock(3, 0)], "MAX_UNLOCK_BATCH");
    chain.block(vec![unlock(2, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 2);
    chain.block(vec![unlock(3, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 3);
}

/// At most one unlock entry per block is a state-independent block rule that merged blocks must satisfy too (D-063,
/// D-074).
#[test]
fn two_unlock_entries_violate_the_static_rules() {
    let one = vec![unlock(1, 0)];
    let two = vec![unlock(1, 0), unlock(2, 0)];
    check_entries_static(&NO_REWARD, &NO_REWARD.rules(0), &one).unwrap();
    let err = check_entries_static(&NO_REWARD, &NO_REWARD.rules(0), &two).unwrap_err();
    assert!(err.contains("at most one"), "{err}");
}

#[test]
fn unlock_position_invalid_conditions() {
    let Setup {
        mut chain,
        a,
        b,
        block1,
    } = setup();
    chain.btc.settle(block1);
    // Nonexistent positions: height 1 accepted only one block; a height that does not exist yet; this block's own
    // height
    assert_invalid(
        &mut chain,
        vec![unlock(1, 1)],
        "not in the unlockable segment",
    );
    assert_invalid(
        &mut chain,
        vec![unlock(99, 0)],
        "not in the unlockable segment",
    );
    assert_invalid(
        &mut chain,
        vec![unlock(2, 0)],
        "not in the unlockable segment",
    );
    // The genesis block accepts no block
    assert_invalid(
        &mut chain,
        vec![unlock(0, 0)],
        "not in the unlockable segment",
    );
    // Order: an unlock entry after a path-B entry is invalid, and so is one before a forced-inclusion entry
    assert_invalid(
        &mut chain,
        vec![
            sponsored(&a, 2, 1, GAS, transfer(b.account, 1)),
            unlock(1, 0),
        ],
        "order",
    );
    assert_invalid(
        &mut chain,
        vec![unlock(1, 0), forced(&a, 2, 100_000, transfer(b.account, 1))],
        "order",
    );
    // After a forced-inclusion entry it is fine
    let out = chain
        .block(vec![
            forced(&a, 2, 100_000, transfer(b.account, 1)),
            unlock(1, 0),
        ])
        .unwrap();
    assert_eq!(out.receipts.len(), 2);
    assert_eq!(chain.locked_fee(&block1), None);
}

#[test]
fn unlock_btc_view_failure_is_fatal_not_invalid() {
    let Setup {
        mut chain, block1, ..
    } = setup();
    chain.btc.settle(block1);
    chain.btc.fail = true;
    let before = chain.state.0.clone();
    let r = chain.block(vec![unlock(1, 0)]);
    assert!(matches!(r, Err(BlockError::Fatal(_))), "{r:?}");
    assert_eq!(chain.state.0, before);
    // The checks depending only on the state and the segment come before querying the view: a position not in the
    // segment is still judged invalid
    invalid_reason(chain.block(vec![unlock(5, 0)]));
    chain.btc.fail = false;
    chain.block(vec![unlock(1, 0)]).unwrap();
    assert_eq!(chain.locked_fee(&block1), None);
    // An already unlocked position does not query the view either
    chain.btc.fail = true;
    invalid_reason(chain.block(vec![unlock(1, 0)]));
}

/// The segment is given by the node or indexer: exceeding `MAX_UNLOCK_BATCH`, not increasing in acceptance order, or
/// disagreeing with the producer of a sponsored-fee record are all their fault (`Fatal`), not an invalid block.
#[test]
fn a_malformed_segment_is_a_local_fault() {
    let Setup {
        mut chain,
        block1,
        a,
        ..
    } = setup();
    chain.block(vec![]).unwrap();
    chain.btc.settle(block1);
    let run = |chain: &Chain, env: &ksana_execution::BlockEnv| {
        chain.exec.execute_block(
            Arc::new(chain.state.clone()),
            Arc::new(chain.btc.clone()),
            env,
            &[unlock(1, 0)],
        )
    };
    let good = chain.env();
    run(&chain, &good).unwrap();

    let mut reversed = good.clone();
    reversed.unlockable.reverse();
    assert!(matches!(run(&chain, &reversed), Err(BlockError::Fatal(_))));

    let mut too_long = good.clone();
    let extra = too_long.unlockable[1].clone();
    for i in 0..chain.exec.params().max_unlock_batch {
        let mut item = extra.clone();
        item.height = 10 + i;
        too_long.unlockable.push(item);
    }
    assert!(matches!(run(&chain, &too_long), Err(BlockError::Fatal(_))));

    let mut wrong_producer = good;
    wrong_producer.unlockable[0].producer = a.account;
    assert!(matches!(
        run(&chain, &wrong_producer),
        Err(BlockError::Fatal(_))
    ));
}

/// Unlock entries of merged blocks are skipped (D-100): only this block's own unlock entry moves the cursor, so a
/// merged unlock entry neither pays out nor stands in the way of this block's own.
#[test]
fn merged_unlocks_leave_the_cursor_to_the_own_entries() {
    let (alice, bob) = (user("alice"), user("bob"));
    let p = user("p");
    let mut chain = Chain::new(vec![(alice.account, 100)]);
    for nonce in 0..2 {
        chain
            .block(vec![sponsored(
                &alice,
                nonce,
                5 + u128::from(nonce),
                GAS,
                transfer(bob.account, 1),
            )])
            .unwrap();
        chain.btc.settle(chain.parent);
    }
    let producer = chain.producer;
    let merged = |seed: &str, entries| MergedBlock {
        hash: sha256(seed.as_bytes()),
        producer: p.account,
        entries,
        fast_path_paused: false,
    };
    // Both merged blocks unlock (one up to (1, 0), one up to (2, 0)) and are skipped; this block unlocking up to (1, 0)
    // is still valid
    let out = chain
        .chain_block(
            vec![
                merged("u1", vec![unlock(1, 0)]),
                merged("u2", vec![unlock(2, 0)]),
            ],
            vec![unlock(1, 0)],
        )
        .unwrap();
    for receipts in &out.merged_receipts {
        assert!(
            matches!(receipts[0].status, OpStatus::Skipped(_)),
            "{:?}",
            receipts[0].status
        );
    }
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert_eq!(chain.ksa(&producer), 5);
    assert_eq!(chain.cursor(), Some((1, 0)));
    // A merged unlock entry alone pays nothing out and leaves the cursor where it is
    let out = chain
        .chain_block(vec![merged("u3", vec![unlock(2, 0)])], vec![])
        .unwrap();
    assert!(matches!(
        out.merged_receipts[0][0].status,
        OpStatus::Skipped(_)
    ));
    assert_eq!(chain.ksa(&producer), 5);
    assert_eq!(chain.cursor(), Some((1, 0)));
    chain.block(vec![unlock(2, 0)]).unwrap();
    assert_eq!(chain.ksa(&producer), 5 + 6);
    assert_eq!(chain.cursor(), Some((2, 0)));
}

#[test]
fn rejected_unlock_leaves_the_built_block_unchanged() {
    let Setup {
        mut chain,
        a,
        b,
        block1,
    } = setup();
    chain.block(vec![]).unwrap();
    chain.btc.settle(block1);
    let env = chain.env();
    let good = unlock(1, 0);
    let unknown = unlock(7, 0);
    let fast = sponsored(&a, 2, 1, GAS, transfer(b.account, 1));
    let late = unlock(1, 0);
    let base = Arc::new(chain.state.clone());
    let btc = Arc::new(chain.btc.clone());
    let (built, rejected) = chain
        .exec
        .build_block(base.clone(), btc.clone(), &env, |builder| {
            let mut rejected = Vec::new();
            for entry in [&unknown, &good, &good, &fast, &late] {
                match builder.push(entry) {
                    Ok(receipt) => assert_eq!(receipt.status, OpStatus::Success),
                    Err(BlockError::Invalid(reason)) => rejected.push(reason),
                    Err(fatal) => return Err(fatal),
                }
            }
            assert_eq!(builder.gas(), GAS, "an unlock entry counts 0 gas");
            Ok(rejected)
        })
        .unwrap();
    assert_eq!(rejected.len(), 3, "{rejected:?}");
    let validated = chain
        .exec
        .execute_block(base, btc, &env, &[good, fast])
        .unwrap();
    assert_eq!(built, validated);
}
