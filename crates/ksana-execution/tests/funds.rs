//! Attached funds on calls (D-073): the `funds` of a `Call` / `Deploy` are transferred from the signing account to the
//! contract before it runs, and contract-to-contract calls pay out of the calling contract's own balance; a failure at
//! any level rolls back the transfer with it; when the rules are not met or the balance is insufficient the op fails
//! (the block stays valid). Contracts read the attached funds with the test contract `probe`.

mod common;

use std::sync::Arc;

use common::probe::*;
use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::OpStatus;
use ksana_primitives::hash::sha256;
use ksana_primitives::{Funds, DEVNET, MAX_CALL_FUNDS};
use ksana_test_contracts::PROBE;
use ksana_vm::gas;

#[test]
fn call_and_deploy_move_funds_into_the_contract_before_it_runs() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);

    // Attached on deploy: already arrived before init runs
    let p = w.deploy(&alice, vec![ksa(500)]);
    assert_eq!(w.chain.ksa(&p), 500);
    assert_eq!(w.chain.ksa(&alice.account), START - 500 - FEE);
    let r = w.run(&alice, call(p, "init_funds", vec![], vec![]));
    assert_eq!(
        borsh::from_slice::<Vec<Funds>>(&r.output).unwrap(),
        vec![ksa(500)]
    );

    // Attached on call: the contract sees the caller and the attached funds
    let r = w.run(&alice, call(p, "funds", vec![], vec![ksa(100)]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let (caller, funds): ([u8; 32], Vec<Funds>) = borsh::from_slice(&r.output).unwrap();
    assert_eq!((caller, funds), (alice.account, vec![ksa(100)]));
    assert_eq!(w.chain.ksa(&p), 600);
    assert_eq!(w.chain.ksa(&alice.account), START - 600 - 3 * FEE);
    // Empty when nothing is attached
    let r = w.run(&alice, call(p, "funds", vec![], vec![]));
    let (_, funds): ([u8; 32], Vec<Funds>) = borsh::from_slice(&r.output).unwrap();
    assert!(funds.is_empty());

    // Several assets: an asset issued by probe attached together with KSA (ascending by asset ID, KSA's ID is the
    // smallest)
    let r = w.run(
        &alice,
        call(p, "issue", enc(&([1u8; 32], 1_000u128)), vec![]),
    );
    let token: [u8; 32] = borsh::from_slice(&r.output).unwrap();
    let q = w.deploy(&alice, vec![]);
    let both = vec![
        ksa(1),
        Funds {
            asset: token,
            amount: 250,
        },
    ];
    let r = w.run(&alice, call(q, "funds", vec![], both.clone()));
    let (_, funds): ([u8; 32], Vec<Funds>) = borsh::from_slice(&r.output).unwrap();
    assert_eq!(funds, both);
    assert_eq!(w.balance(&token, &q), 250);
    assert_eq!(w.balance(&token, &alice.account), 750);
    assert_eq!(w.chain.ksa(&q), 1);
}

#[test]
fn failures_roll_back_the_attached_funds() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    let before = w.chain.ksa(&alice.account);

    // Contract revert: the attached transfer is rolled back with it, only the fee is charged
    let r = w.run(&alice, call(p, "revert", vec![], vec![ksa(100)]));
    assert!(failed(&r).contains("probe revert"), "{r:?}");
    assert_eq!(w.chain.ksa(&alice.account), before - FEE);
    assert_eq!(w.chain.ksa(&p), 0);

    // init fails: both the deployment and the attached transfer are undone
    let r = w.run(&alice, deploy_probe(b"fail", vec![ksa(100)]));
    failed(&r);
    let nonce = w.nonces[&alice.account] - 1;
    let id = contract_id(&alice.account, nonce, &sha256(PROBE));
    assert!(w
        .chain
        .get::<Vec<u8>>(&ksana_execution::keys::contract(&id))
        .is_none());
    assert_eq!(w.chain.ksa(&id), 0);
    assert_eq!(w.chain.ksa(&alice.account), before - 2 * FEE);

    // Insufficient balance (the KSA balance already had this op's sponsored fee deducted), nonexistent asset
    let have = w.chain.ksa(&alice.account) - FEE;
    let r = w.run(&alice, call(p, "noop", vec![], vec![ksa(have + 1)]));
    assert!(
        failed(&r).contains("attached funds transfer failed"),
        "{r:?}"
    );
    let r = w.run(
        &alice,
        call(
            p,
            "noop",
            vec![],
            vec![Funds {
                asset: [7; 32],
                amount: 1,
            }],
        ),
    );
    failed(&r);
    assert_eq!(w.chain.ksa(&alice.account), before - 4 * FEE);
    // Using up the balance exactly succeeds
    let have = w.chain.ksa(&alice.account) - FEE;
    let r = w.run(&alice, call(p, "noop", vec![], vec![ksa(have)]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(w.chain.ksa(&alice.account), 0);
}

#[test]
fn malformed_funds_fail_the_operation_not_the_block() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    let x = |amount| Funds {
        asset: [5; 32],
        amount,
    };
    let too_many: Vec<Funds> = (0..=MAX_CALL_FUNDS)
        .map(|i| Funds {
            asset: [u8::try_from(i).unwrap(); 32],
            amount: 1,
        })
        .collect();
    for funds in [
        vec![x(1), ksa(1)],
        vec![ksa(1), ksa(1)],
        vec![ksa(0)],
        too_many,
    ] {
        let before = w.chain.ksa(&alice.account);
        let r = w.run(&alice, call(p, "noop", vec![], funds.clone()));
        assert_eq!(r.gas_used, GAS, "{r:?}");
        failed(&r);
        let r = w.run(&alice, deploy_probe(b"ok", funds));
        failed(&r);
        assert_eq!(w.chain.ksa(&alice.account), before - 2 * FEE);
    }
}

#[test]
fn each_attached_asset_costs_fixed_gas() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    let r = w.run(&alice, call(p, "issue", enc(&([1u8; 32], 10u128)), vec![]));
    let token: [u8; 32] = borsh::from_slice(&r.output).unwrap();

    let plain = w.run(&alice, call(p, "noop", vec![], vec![])).gas_used;
    let one = w
        .run(&alice, call(p, "noop", vec![], vec![ksa(1)]))
        .gas_used;
    let two = w
        .run(
            &alice,
            call(
                p,
                "noop",
                vec![],
                vec![
                    ksa(1),
                    Funds {
                        asset: token,
                        amount: 1,
                    },
                ],
            ),
        )
        .gas_used;
    assert_eq!(one - plain, gas::FUNDS_PER_ASSET);
    assert_eq!(two - plain, 2 * gas::FUNDS_PER_ASSET);

    // gas only enough for the intrinsic cost: fails; the cost of attached funds is charged before the contract runs
    let r = w.run_gas(
        &alice,
        gas::CALL_BASE + gas::FUNDS_PER_ASSET - 1,
        call(p, "noop", vec![], vec![ksa(1)]),
    );
    assert!(failed(&r).contains("gas"), "{r:?}");
    assert_eq!(w.chain.ksa(&p), 2);
}

#[test]
fn contracts_attach_funds_from_their_own_balance() {
    let alice = user("alice");
    let bob = user("bob");
    let mut w = World::new(&[&alice, &bob]);
    let p = w.deploy(&alice, vec![]);
    let q = w.deploy(&alice, vec![]);

    // alice → p (50 attached) → q (30 attached, out of p's balance): q sees p as the caller
    let r = w.run(
        &alice,
        call(
            p,
            "forward",
            forward(q, "funds", vec![], vec![ksa(30)]),
            vec![ksa(50)],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let (caller, funds): ([u8; 32], Vec<Funds>) = borsh::from_slice(&r.output).unwrap();
    assert_eq!((caller, funds), (p, vec![ksa(30)]));
    assert_eq!((w.chain.ksa(&p), w.chain.ksa(&q)), (20, 30));

    // p's own balance is insufficient: fails even though alice's balance suffices, everything rolled back
    let alice_before = w.chain.ksa(&alice.account);
    let r = w.run(
        &alice,
        call(
            p,
            "forward",
            forward(q, "noop", vec![], vec![ksa(21)]),
            vec![],
        ),
    );
    failed(&r);
    assert_eq!((w.chain.ksa(&p), w.chain.ksa(&q)), (20, 30));
    assert_eq!(w.chain.ksa(&alice.account), alice_before - FEE);

    // The caller reverts after a successful sub-call: the attached transfers of both levels are rolled back
    let r = w.run(
        &alice,
        call(
            p,
            "forward_then_revert",
            forward(q, "noop", vec![], vec![ksa(5)]),
            vec![ksa(7)],
        ),
    );
    assert!(failed(&r).contains("probe revert"), "{r:?}");
    assert_eq!((w.chain.ksa(&p), w.chain.ksa(&q)), (20, 30));
    assert_eq!(w.chain.ksa(&alice.account), alice_before - 2 * FEE);

    // A contract can only move its own balance (D-016): p sending more KSA than it holds fails, the caller's balance is
    // unaffected
    let r = w.run(
        &bob,
        call(p, "send", enc(&(KSA, bob.account, 21u128)), vec![]),
    );
    failed(&r);
    let r = w.run(
        &bob,
        call(p, "send", enc(&(KSA, bob.account, 20u128)), vec![]),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(w.chain.ksa(&bob.account), START - 2 * FEE + 20);
    assert_eq!(w.chain.ksa(&p), 0);
}

#[test]
fn forced_entries_attach_funds_too() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    let nonce = w.nonces[&alice.account];
    let out = w
        .chain
        .block(vec![forced(
            &alice,
            nonce,
            DEVNET.native_max_gas,
            call(p, "funds", vec![], vec![ksa(9)]),
        )])
        .unwrap();
    assert_eq!(out.receipts[0].status, OpStatus::Success);
    assert_eq!(w.chain.ksa(&p), 9);
}

#[test]
fn read_only_calls_simulate_the_funds() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    let state = w.chain.state.clone();
    let simulate = |caller: [u8; 32], funds: Vec<Funds>| {
        w.chain
            .exec
            .simulate_call(
                Arc::new(state.clone()),
                Arc::new(ksana_execution::NoBtc),
                &w.chain.env(),
                caller,
                p,
                "funds".into(),
                vec![],
                funds,
                GAS,
            )
            .unwrap()
    };
    let r = simulate(alice.account, vec![ksa(5)]);
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    let (caller, funds): ([u8; 32], Vec<Funds>) = borsh::from_slice(&r.output).unwrap();
    assert_eq!((caller, funds), (alice.account, vec![ksa(5)]));
    // A caller without balance, attached funds violating the rules: failure
    assert!(matches!(
        simulate([3; 32], vec![ksa(5)]).status,
        OpStatus::Failed(_)
    ));
    assert!(matches!(
        simulate(alice.account, vec![ksa(0)]).status,
        OpStatus::Failed(_)
    ));
    assert_eq!(
        w.chain.state.0, state.0,
        "a read-only call does not change state"
    );
}
