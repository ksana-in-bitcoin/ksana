//! Execution rules: fee, nonce and failure semantics of paths B / C (for path C see the revision of D-018), block
//! invalidity conditions, sponsored-fee records (D-079), block gas (D-036).

mod common;

use std::sync::Arc;

use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::keys::{self, LockedFee};
use ksana_execution::{BlockError, GenesisConfig, OpStatus};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Entry, FeeMode, DEVNET};

const GAS: u64 = 5_000_000;

fn failed(status: &OpStatus) -> bool {
    matches!(status, OpStatus::Failed(_))
}

#[test]
fn sponsored_transfer_charges_the_fee_into_the_block_record_and_increments_nonce() {
    let (a, b) = (user("a"), user("b"));
    let mut chain = Chain::new(vec![(a.account, 1_000)]);
    let out = chain
        .block(vec![sponsored(&a, 0, 10, GAS, transfer(b.account, 100))])
        .unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert_eq!(chain.ksa(&a.account), 1_000 - 10 - 100);
    assert_eq!(chain.ksa(&b.account), 100);
    assert_eq!(chain.nonce(&a.account), 1);
    // The sponsored fee is recorded at this block's acceptance position (1, 0) (D-079)
    let record: LockedFee = chain.get(&keys::locked_fee(1, 0)).unwrap();
    assert_eq!(
        record,
        LockedFee {
            producer: chain.producer,
            amount: 10,
        }
    );
}

#[test]
fn fees_of_one_block_accumulate_in_one_record_that_later_blocks_leave_alone() {
    let a = user("a");
    let mut chain = Chain::new(vec![(a.account, 1_000)]);
    chain
        .block(vec![
            sponsored(&a, 0, 7, GAS, transfer(a.account, 1)),
            sponsored(&a, 1, 5, GAS, transfer(a.account, 1)),
        ])
        .unwrap();
    let block1 = chain.parent;
    let out = chain.block(vec![]).unwrap();
    assert!(out.writes.is_empty(), "an empty block writes no state");
    let record = chain.locked_fee(&block1).unwrap();
    assert_eq!(record.amount, 12);
    assert_eq!(record.producer, chain.producer);
    assert_eq!(chain.locked_fee(&chain.parent), None);
}

#[test]
fn sponsored_action_failure_keeps_fee_and_nonce() {
    let (a, b) = (user("a"), user("b"));
    let mut chain = Chain::new(vec![(a.account, 1_000)]);
    // Transfer amount exceeds the balance: the action fails, the fee is still charged and the nonce incremented
    let out = chain
        .block(vec![sponsored(&a, 0, 10, GAS, transfer(b.account, 5_000))])
        .unwrap();
    assert!(failed(&out.receipts[0].status));
    assert_eq!(out.receipts[0].gas_used, GAS);
    assert_eq!(chain.ksa(&a.account), 990);
    assert_eq!(chain.ksa(&b.account), 0);
    assert_eq!(chain.nonce(&a.account), 1);
}

#[test]
fn contract_trap_and_out_of_gas_roll_back_state_but_keep_fee_and_nonce() {
    let a = user("a");
    let mut chain = Chain::new(vec![(a.account, 1_000)]);
    let trap_code = trap_after_write_contract();
    let spin_code = spin_after_write_contract();
    chain
        .block(vec![
            sponsored(&a, 0, 1, GAS, deploy(trap_code.clone())),
            sponsored(&a, 1, 1, GAS, deploy(spin_code.clone())),
        ])
        .unwrap();
    let trap_id = contract_id(&a.account, 0, &sha256(&trap_code));
    let spin_id = contract_id(&a.account, 1, &sha256(&spin_code));
    let call = |id| Action::Call {
        contract: id,
        method: "go".into(),
        args: vec![],
        funds: Vec::new(),
    };
    let out = chain
        .block(vec![
            sponsored(&a, 2, 3, GAS, call(trap_id)),
            sponsored(&a, 3, 4, 200_000, call(spin_id)),
        ])
        .unwrap();
    for r in &out.receipts {
        assert!(failed(&r.status), "{r:?}");
    }
    assert!(format!("{:?}", out.receipts[1].status).contains("gas"));
    // The storage written by the contract is rolled back
    assert!(!chain.state.0.contains_key(&keys::storage(&trap_id, b"key")));
    assert!(!chain.state.0.contains_key(&keys::storage(&spin_id, b"key")));
    // Nonce incremented, fees charged: 1 + 1 + 3 + 4
    assert_eq!(chain.nonce(&a.account), 4);
    assert_eq!(chain.ksa(&a.account), 1_000 - 9);
}

#[test]
fn successful_call_commits_contract_storage() {
    let a = user("a");
    let mut chain = Chain::new(vec![(a.account, 1_000)]);
    let code = writer_contract();
    chain
        .block(vec![sponsored(&a, 0, 1, GAS, deploy(code.clone()))])
        .unwrap();
    let id = contract_id(&a.account, 0, &sha256(&code));
    assert!(chain.state.0.contains_key(&keys::contract(&id)));
    assert!(chain.state.0.contains_key(&keys::code(&sha256(&code))));
    // init wrote storage too
    assert_eq!(
        chain.state.0.get(&keys::storage(&id, b"key")),
        Some(&b"val".to_vec())
    );
}

#[test]
fn float_contract_is_rejected_at_deploy() {
    let a = user("a");
    let mut chain = Chain::new(vec![(a.account, 1_000)]);
    let code = float_contract();
    let out = chain
        .block(vec![sponsored(&a, 0, 1, GAS, deploy(code.clone()))])
        .unwrap();
    match &out.receipts[0].status {
        OpStatus::Failed(reason) => assert!(reason.contains("deployment rejected"), "{reason}"),
        other => panic!("a floating-point contract should be rejected at deploy: {other:?}"),
    }
    let id = contract_id(&a.account, 0, &sha256(&code));
    assert!(!chain.state.0.contains_key(&keys::contract(&id)));
    assert!(!chain.state.0.contains_key(&keys::code(&sha256(&code))));
    assert_eq!(
        chain.nonce(&a.account),
        1,
        "a failed deployment still increments the nonce"
    );
    assert_eq!(
        chain.ksa(&a.account),
        999,
        "a failed deployment is still charged"
    );
}

#[test]
fn sponsored_block_invalid_conditions() {
    let (a, b) = (user("a"), user("b"));
    let fresh = || Chain::new(vec![(a.account, 100)]);
    let invalid = |r: Result<_, BlockError>| matches!(r, Err(BlockError::Invalid(_)));

    // Non-consecutive nonce
    assert!(invalid(fresh().block(vec![sponsored(
        &a,
        1,
        1,
        GAS,
        transfer(b.account, 1)
    )])));
    // Token balance insufficient for the fee
    assert!(invalid(fresh().block(vec![sponsored(
        &a,
        0,
        101,
        GAS,
        transfer(b.account, 1)
    )])));
    // Invalid signature (tampered amount)
    let mut bad = sponsored(&a, 0, 1, GAS, transfer(b.account, 1));
    if let Entry::Sponsored { op } = &mut bad {
        op.fee = FeeMode::Token { amount: 2 };
    }
    assert!(invalid(fresh().block(vec![bad])));
    // Fee mode is not Token
    let wrong_fee = Entry::Sponsored {
        op: native_op(&a, 0, [0x51; 32], GAS, transfer(b.account, 1)),
    };
    assert!(invalid(fresh().block(vec![wrong_fee])));
    // A failed block does not change state
    let mut chain = fresh();
    let before = chain.state.0.clone();
    assert!(chain
        .block(vec![
            sponsored(&a, 0, 1, GAS, transfer(b.account, 1)),
            sponsored(&a, 2, 1, GAS, transfer(b.account, 1)),
        ])
        .is_err());
    assert_eq!(chain.state.0, before);
}

#[test]
fn block_structure_rules() {
    let a = user("a");
    let mut chain = Chain::new(vec![(a.account, 100)]);
    // Forced-inclusion entries must come first
    let r = chain.block(vec![
        sponsored(&a, 0, 1, GAS, transfer(a.account, 1)),
        forced(&a, 1, GAS, transfer(a.account, 1)),
    ]);
    assert!(matches!(r, Err(BlockError::Invalid(_))));
    // Total gas must not exceed BLOCK_GAS_LIMIT (accumulated by max_gas)
    let r = chain.block(vec![
        sponsored(&a, 0, 1, 60_000_000, transfer(a.account, 1)),
        sponsored(&a, 1, 1, 60_000_000, transfer(a.account, 1)),
    ]);
    assert!(matches!(r, Err(BlockError::Invalid(_))));
}

#[test]
fn forced_failures_consume_the_nonce_only_when_authenticated() {
    let (a, b, c) = (user("a"), user("b"), user("c"));
    let mut chain = Chain::new(vec![(a.account, 1_000), (c.account, 5_000_000)]);
    // Success: nonce incremented, transfer takes effect, no fee paid to the producer
    let out = chain
        .block(vec![forced(&a, 0, 100_000, transfer(b.account, 10))])
        .unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert_eq!(chain.nonce(&a.account), 1);
    assert_eq!(chain.ksa(&b.account), 10);
    assert_eq!(chain.locked_fee(&chain.parent), None);

    // Did not pass authentication (invalid signature, nonce mismatch): recorded as failed, state unchanged, the block
    // stays valid
    let before = chain.state.0.clone();
    let mut tampered = forced(&a, 1, 100_000, transfer(b.account, 1));
    if let Entry::Forced { op, .. } = &mut tampered {
        op.max_gas = 100_001;
    }
    let out = chain
        .block(vec![
            forced(&a, 5, 100_000, transfer(b.account, 1)),
            tampered,
        ])
        .unwrap();
    assert!(out.receipts.iter().all(|r| failed(&r.status)));
    assert_eq!(chain.state.0, before);

    // Authenticated with the correct nonce: both an execution failure and an oversized max_gas consume the nonce, the
    // action does not take effect (revision of D-018, same as paths A and B)
    let overdraft = forced(&a, 1, 100_000, transfer(b.account, 1_000_000));
    let out = chain
        .block(vec![
            overdraft.clone(),
            forced(&a, 2, 2_000_000, transfer(b.account, 1)),
        ])
        .unwrap();
    assert!(out.receipts.iter().all(|r| failed(&r.status)));
    assert_eq!(chain.nonce(&a.account), 3);
    assert_eq!(chain.ksa(&b.account), 10);

    // A failed native op is public: once a's balance suffices later, resubmitting it still does not execute it
    chain
        .block(vec![forced(&c, 0, 100_000, transfer(a.account, 2_000_000))])
        .unwrap();
    let out = chain.block(vec![overdraft]).unwrap();
    assert_eq!(
        out.receipts[0].status,
        OpStatus::Failed("nonce mismatch".into())
    );
    assert_eq!(chain.ksa(&b.account), 10);
}

/// Forced-inclusion entries count `min(max_gas, NATIVE_MAX_GAS)` towards the block gas but are themselves not bound by
/// `BLOCK_GAS_LIMIT`; once the total is over the limit, no other entry (including unlock entries counting 0 gas) can be
/// added (D-036).
#[test]
fn forced_entries_are_not_limited_by_the_block_gas_limit() {
    let (a, b) = (user("a"), user("b"));
    let mut chain = Chain::new(vec![(a.account, 1_000), (b.account, 1_000)]);
    let native = DEVNET.native_max_gas;
    let count = DEVNET.block_gas_limit / native + 1;
    let forced_entries: Vec<Entry> = (0..count)
        .map(|n| forced(&a, n, native, transfer(b.account, 1)))
        .collect();
    let later = [sponsored(&b, 0, 1, 1, transfer(a.account, 1))];
    // An unlock entry counts 0 gas and is not bound by the limit: here it is invalid because the position is not in the
    // unlockable segment, not because of gas
    let zero_gas = unlock(7, 0);

    let env = chain.env();
    let (built, ()) = chain
        .exec
        .build_block(
            Arc::new(chain.state.clone()),
            Arc::new(chain.btc.clone()),
            &env,
            |builder| {
                for entry in &forced_entries {
                    assert_eq!(builder.push(entry)?.status, OpStatus::Success);
                }
                assert_eq!(builder.gas(), count * native);
                assert!(builder.gas() > DEVNET.block_gas_limit);
                for entry in &later {
                    match builder.push(entry) {
                        Err(BlockError::Invalid(reason)) => {
                            assert!(reason.contains("BLOCK_GAS_LIMIT"), "{reason}")
                        }
                        other => panic!("{entry:?} should be rejected once the total exceeds the limit: {other:?}"),
                    }
                }
                match builder.push(&zero_gas) {
                    Err(BlockError::Invalid(reason)) => {
                        assert!(!reason.contains("BLOCK_GAS_LIMIT"), "{reason}")
                    }
                    other => panic!("an unlock entry not in the unlockable segment should be invalid: {other:?}"),
                }
                assert_eq!(builder.gas(), count * native);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(u64::try_from(built.receipts.len()).unwrap(), count);

    // Validation agrees: a block with only forced-inclusion entries is valid, adding any other entry makes it invalid
    for entry in later {
        let mut entries = forced_entries.clone();
        entries.push(entry);
        let mut fresh = Chain::new(vec![(a.account, 1_000), (b.account, 1_000)]);
        fresh.btc = chain.btc.clone();
        match fresh.block(entries) {
            Err(BlockError::Invalid(reason)) => {
                assert!(reason.contains("BLOCK_GAS_LIMIT"), "{reason}")
            }
            other => panic!("the block should be invalid: {other:?}"),
        }
    }
    let out = chain.block(forced_entries).unwrap();
    assert_eq!(out, built);
    assert_eq!(chain.nonce(&a.account), count);
    assert_eq!(chain.ksa(&b.account), 1_000 + u128::from(count));
}

/// A forced-inclusion entry with `max_gas` above `NATIVE_MAX_GAS` counts only `NATIVE_MAX_GAS` (it is not executed and
/// recorded as failed); the block is still valid when the other entries bring the total to exactly the limit.
#[test]
fn oversized_forced_op_counts_only_native_max_gas() {
    let a = user("a");
    let chain = Chain::new(vec![(a.account, 1_000)]);
    let (limit, native) = (DEVNET.block_gas_limit, DEVNET.native_max_gas);
    // The oversized native op has a correct signature and nonce: not executed, but nonce 0 is consumed (revision of
    // D-018)
    let huge = forced(&a, 0, u64::MAX, transfer(a.account, 1));
    let fill = sponsored(&a, 1, 1, limit - native, transfer(a.account, 1));
    let one_more = sponsored(&a, 2, 1, 1, transfer(a.account, 1));
    let env = chain.env();
    let (built, ()) = chain
        .exec
        .build_block(
            Arc::new(chain.state.clone()),
            Arc::new(chain.btc.clone()),
            &env,
            |builder| {
                let receipt = builder.push(&huge)?;
                assert!(
                    matches!(&receipt.status, OpStatus::Failed(r) if r.contains("NATIVE_MAX_GAS"))
                );
                assert_eq!(builder.gas(), native);
                builder.push(&fill)?;
                assert_eq!(builder.gas(), limit, "exactly at the limit");
                assert!(matches!(
                    builder.push(&one_more),
                    Err(BlockError::Invalid(_))
                ));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(built.receipts.len(), 2);
    let run = |entries: &[Entry]| {
        chain.exec.execute_block(
            Arc::new(chain.state.clone()),
            Arc::new(chain.btc.clone()),
            &env,
            entries,
        )
    };
    assert_eq!(run(&[huge.clone(), fill.clone()]).unwrap(), built);
    assert!(matches!(
        run(&[huge, fill, one_more]),
        Err(BlockError::Invalid(_))
    ));
}

#[test]
fn genesis_config() {
    let (a, b) = (user("a"), user("b"));
    let writes = GenesisConfig::devnet(vec![(a.account, 5), (b.account, 7)])
        .writes()
        .unwrap();
    let meta: keys::AssetMeta =
        borsh::from_slice(writes[&keys::asset(&KSA)].as_ref().unwrap()).unwrap();
    assert_eq!(meta.total_supply, 12);
    assert_eq!(meta.name, "KSA");
    // KSA's symbol, and no issuing contract or capabilities (D-121)
    assert_eq!(
        (
            meta.symbol.as_str(),
            meta.issuer,
            meta.capabilities,
            meta.paused
        ),
        ("KSA", [0u8; 32], 0, false)
    );
    assert!(GenesisConfig::devnet(vec![(a.account, 5), (a.account, 7)])
        .writes()
        .is_err());
}
