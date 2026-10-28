//! The producer's entry-by-entry block building (`Executor::build_block`) and read-only calls
//! (`Executor::simulate_call`).

mod common;

use std::sync::Arc;

use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::{BlockError, NoBtc, OpStatus};
use ksana_primitives::hash::sha256;
use ksana_primitives::Action;

const GAS: u64 = 5_000_000;

#[test]
fn rejected_entries_leave_the_built_block_unchanged() {
    let (a, b) = (user("a"), user("b"));
    let chain = Chain::new(vec![(a.account, 1_000), (b.account, 50)]);
    let env = chain.env();
    let ok1 = sponsored(&a, 0, 3, GAS, transfer(b.account, 10));
    let gap = sponsored(&a, 5, 3, GAS, transfer(b.account, 10));
    // After receiving 10 from ok1, b has 60 in total and still cannot pay a sponsored fee of 100
    let poor = sponsored(&b, 0, 100, GAS, transfer(a.account, 1));
    let too_much_gas = sponsored(&a, 1, 3, 200_000_000, transfer(b.account, 10));
    let ok2 = sponsored(&a, 1, 4, GAS, transfer(b.account, 20));
    let late_forced = forced(&b, 0, 100_000, transfer(a.account, 1));

    let (built, rejected) = chain
        .exec
        .build_block(
            Arc::new(chain.state.clone()),
            Arc::new(NoBtc),
            &env,
            |builder| {
                let mut rejected = Vec::new();
                for entry in [&ok1, &gap, &poor, &too_much_gas, &ok2, &late_forced] {
                    match builder.push(entry) {
                        Ok(receipt) => assert_eq!(receipt.status, OpStatus::Success),
                        Err(BlockError::Invalid(reason)) => rejected.push(reason),
                        Err(fatal) => return Err(fatal),
                    }
                }
                assert_eq!(builder.gas(), 2 * GAS);
                assert_eq!(builder.receipts().len(), 2);
                Ok(rejected)
            },
        )
        .unwrap();
    assert_eq!(rejected.len(), 4, "{rejected:?}");

    // Identical to the result of directly validating a block containing only the accepted entries
    let validated = chain
        .exec
        .execute_block(
            Arc::new(chain.state.clone()),
            Arc::new(NoBtc),
            &env,
            &[ok1, ok2],
        )
        .unwrap();
    assert_eq!(built, validated);
}

#[test]
fn simulate_call_returns_output_without_state_changes() {
    let a = user("a");
    let mut chain = Chain::new(vec![(a.account, 1_000)]);
    let code = writer_contract();
    chain
        .block(vec![sponsored(&a, 0, 1, GAS, deploy(code.clone()))])
        .unwrap();
    let id = contract_id(&a.account, 0, &sha256(&code));
    let before = chain.state.clone();

    let receipt = chain
        .exec
        .simulate_call(
            Arc::new(chain.state.clone()),
            Arc::new(NoBtc),
            &chain.env(),
            a.account,
            id,
            "go".into(),
            vec![],
            Vec::new(),
            GAS,
        )
        .unwrap();
    assert_eq!(receipt.status, OpStatus::Success);
    assert!(receipt.gas_used > 0 && receipt.gas_used < GAS);
    assert_eq!(receipt.op_id, None);
    assert_eq!(
        chain.state.0, before.0,
        "a read-only call does not change state"
    );

    // Missing contract, insufficient gas: failed receipts
    let missing = chain
        .exec
        .simulate_call(
            Arc::new(chain.state.clone()),
            Arc::new(NoBtc),
            &chain.env(),
            a.account,
            [7; 32],
            "go".into(),
            vec![],
            Vec::new(),
            GAS,
        )
        .unwrap();
    assert!(matches!(missing.status, OpStatus::Failed(_)));
    let starved = chain
        .exec
        .simulate_call(
            Arc::new(chain.state.clone()),
            Arc::new(NoBtc),
            &chain.env(),
            a.account,
            id,
            "go".into(),
            vec![],
            Vec::new(),
            100,
        )
        .unwrap();
    assert!(matches!(starved.status, OpStatus::Failed(_)));
    // Confirm via the Action form that the call also succeeds when included in a block
    let out = chain
        .block(vec![sponsored(
            &a,
            1,
            1,
            GAS,
            Action::Call {
                contract: id,
                method: "go".into(),
                args: vec![],
                funds: Vec::new(),
            },
        )])
        .unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert_eq!(out.receipts[0].gas_used, receipt.gas_used);
}
