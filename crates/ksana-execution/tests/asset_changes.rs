//! A receipt lists the changes its op made to built-in assets (D-122), in the order the op made them, each after the
//! events recorded before it: the `AssetTransfer` action, the funds attached to a `Call`, a `Deploy` or the RPC's
//! read-only `ksana_call`, a contract's `asset_transfer` and the funds it attaches to `call_contract`,
//! `try_call_contract` and `create_contract`, and D-121's issuance and management functions. KSA's fees are not
//! changes, nor is a movement of 0; a failed op has none, and a level `try_call_contract` reverts takes its own with
//! it. The contracts are the test contract `probe`, whose methods anyone may call, and the reference KS-20 token with
//! the receiving contract of the standards' tests.

mod common;

use std::sync::Arc;

use common::probe::*;
use common::*;
use ksana_execution::builtin::contract_id;
use ksana_execution::{AssetChange, ChangeKind, NoBtc, OpStatus, Receipt};
use ksana_primitives::asset::{CAP_ALLOWLIST, CAP_FREEZE, CAP_MINT, CAP_PAUSE};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Funds};
use ksana_test_contracts::{PROBE, RECEIVER, TOKEN};
use ksana_vm::AssetMetaInput;

fn ok(r: &Receipt) -> &[u8] {
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    &r.output
}

/// The changes of an op that succeeded.
fn changes(r: &Receipt) -> &[AssetChange] {
    ok(r);
    &r.asset_changes
}

fn change(events: u32, asset: [u8; 32], kind: ChangeKind) -> AssetChange {
    AssetChange {
        events,
        asset,
        kind,
    }
}

fn funds(from: [u8; 32], to: [u8; 32], amount: u128) -> ChangeKind {
    ChangeKind::Funds { from, to, amount }
}

fn payout(from: [u8; 32], to: [u8; 32], amount: u128) -> ChangeKind {
    ChangeKind::Payout { from, to, amount }
}

fn send(to: [u8; 32], amount: u128) -> Action {
    Action::AssetTransfer {
        asset: KSA,
        to,
        amount,
    }
}

/// The arguments of `probe.try_forward`.
fn try_forward(target: [u8; 32], method: &str, inner: Vec<u8>, funds: Vec<Funds>) -> Vec<u8> {
    enc(&(target, method.to_string(), inner, funds, 5_000_000u64))
}

/// Deploys `code` with `init_args` and returns the contract ID.
fn deploy(w: &mut World, u: &User, code: &[u8], init_args: Vec<u8>) -> [u8; 32] {
    let nonce = w.nonces.get(&u.account).copied().unwrap_or(0);
    let action = Action::Deploy {
        code: code.to_vec(),
        init_args,
        funds: Vec::new(),
    };
    ok(&w.run(u, action));
    contract_id(&u.account, nonce, &sha256(code))
}

#[test]
fn the_actions_record_what_they_move_and_a_failed_op_nothing() {
    let (alice, bob) = (user("alice"), user("bob"));
    let (a, b) = (alice.account, bob.account);
    let mut w = World::new(&[&alice, &bob]);

    // the AssetTransfer action; its fee is no change
    let r = w.run(&alice, send(b, 5));
    assert_eq!(
        changes(&r),
        [change(
            0,
            KSA,
            ChangeKind::Transfer {
                from: a,
                to: b,
                amount: 5
            }
        )]
    );

    // the funds attached to a deployment and to a call
    let nonce = w.nonces[&a];
    let r = w.run(&alice, deploy_probe(b"ok", vec![ksa(3)]));
    let probe = contract_id(&a, nonce, &sha256(PROBE));
    assert_eq!(changes(&r), [change(0, KSA, funds(a, probe, 3))]);
    let r = w.run(&bob, call(probe, "noop", vec![], vec![ksa(2)]));
    assert_eq!(changes(&r), [change(0, KSA, funds(b, probe, 2))]);
    assert_eq!(w.balance(&KSA, &probe), 5);

    // the read-only call of the RPC's ksana_call lists the same changes as the call it tries
    let chain = &w.chain;
    let r = chain
        .exec
        .simulate_call(
            Arc::new(chain.state.clone()),
            Arc::new(NoBtc),
            &chain.env(),
            b,
            probe,
            "noop".into(),
            vec![],
            vec![ksa(2)],
            GAS,
        )
        .unwrap();
    assert_eq!(changes(&r), [change(0, KSA, funds(b, probe, 2))]);

    // a movement of 0 changes nothing
    let r = w.run(&alice, send(b, 0));
    assert_eq!(changes(&r), []);

    // a failed op: the funds went back with everything else, and the receipt has no changes
    let r = w.run(&bob, call(probe, "revert", vec![], vec![ksa(2)]));
    failed(&r);
    assert!(r.asset_changes.is_empty(), "{r:?}");
    let r = w.run(&bob, send(a, START));
    assert_eq!(failed(&r), "insufficient balance");
    assert!(r.asset_changes.is_empty(), "{r:?}");
    assert_eq!(w.balance(&KSA, &probe), 5);
}

#[test]
fn contracts_record_their_payouts_and_the_funds_they_attach() {
    let (alice, bob) = (user("alice"), user("bob"));
    let b = bob.account;
    let mut w = World::new(&[&alice, &bob]);
    let p = w.deploy(&alice, vec![ksa(100)]);
    let q = w.deploy(&alice, vec![ksa(100)]);
    let r_ = w.deploy(&alice, vec![]);

    // P attaches 10 to a call of Q, which pays 4 to Bob
    let inner = enc(&(KSA, b, 4u128));
    let args = forward(q, "send", inner.clone(), vec![ksa(10)]);
    let r = w.run(&bob, call(p, "forward", args, vec![]));
    assert_eq!(
        changes(&r),
        [
            change(0, KSA, funds(p, q, 10)),
            change(0, KSA, payout(q, b, 4))
        ]
    );

    // and to a contract it creates
    let args = enc(&(sha256(PROBE), [9u8; 32], enc(&b"ok".to_vec()), vec![ksa(5)]));
    let r = w.run(&bob, call(p, "create", args, vec![]));
    let created: [u8; 32] = borsh::from_slice(ok(&r)).unwrap();
    assert_eq!(changes(&r), [change(0, KSA, funds(p, created, 5))]);

    // try_call_contract: a level that reverts takes its funds and the funds it attached further back with it, while
    // the event P emits afterwards stays
    let args = try_forward(
        q,
        "forward_then_revert",
        forward(r_, "noop", vec![], vec![ksa(1)]),
        vec![ksa(2)],
    );
    let r = w.run(&bob, call(p, "try_forward", args, vec![]));
    let (status, _): (u8, Vec<u8>) = borsh::from_slice(ok(&r)).unwrap();
    assert_eq!(status, 1, "the level reverted");
    assert_eq!(changes(&r), []);
    assert_eq!(r.events.len(), 1);
    assert_eq!(w.balance(&KSA, &r_), 0);
    // a level that succeeds keeps them, before that event
    let args = try_forward(q, "send", inner, vec![ksa(2)]);
    let r = w.run(&bob, call(p, "try_forward", args, vec![]));
    let (status, _): (u8, Vec<u8>) = borsh::from_slice(ok(&r)).unwrap();
    assert_eq!(status, 0);
    assert_eq!(
        changes(&r),
        [
            change(0, KSA, funds(p, q, 2)),
            change(0, KSA, payout(q, b, 4))
        ]
    );
    assert_eq!(r.events.len(), 1);

    // a level that fails at the top fails the op
    let args = forward(q, "send", enc(&(KSA, b, 1u128)), vec![ksa(1)]);
    let r = w.run(&bob, call(p, "forward_then_revert", args, vec![]));
    failed(&r);
    assert!(r.asset_changes.is_empty(), "{r:?}");
}

#[test]
fn changes_and_events_interleave_in_the_order_the_op_made_them() {
    let alice = user("alice");
    let a = alice.account;
    let mut w = World::new(&[&alice]);
    let init = enc(&("T".to_string(), "T".to_string(), 0u8, 1_000u128));
    let token = deploy(&mut w, &alice, TOKEN, init);
    let receiver = deploy(&mut w, &alice, RECEIVER, Vec::new());

    // transfer_and_call emits the token's transfer event, then passes the funds attached to it on to the recipient:
    // the second change comes after that event
    let args = enc(&(receiver, 100u128, b"hi".to_vec()));
    let r = w.run(
        &alice,
        call(token, "transfer_and_call", args, vec![ksa(50)]),
    );
    assert_eq!(r.events.len(), 1);
    assert_eq!(r.events[0].contract, token);
    assert_eq!(
        changes(&r),
        [
            change(0, KSA, funds(a, token, 50)),
            change(1, KSA, funds(token, receiver, 50))
        ]
    );
    assert_eq!(w.balance(&KSA, &receiver), 50);
}

#[test]
fn an_issuer_s_actions_on_its_asset_are_recorded_with_what_they_did() {
    let (alice, bob) = (user("alice"), user("bob"));
    let (a, b) = (alice.account, bob.account);
    let mut w = World::new(&[&alice, &bob]);
    let issuer = w.deploy(&alice, vec![]);

    // the first asset_issue creates the asset with `mint` alone; later ones only issue, and an issue of 0 is nothing
    let sub_id = [1u8; 32];
    let r = w.run(
        &alice,
        call(issuer, "issue", enc(&(sub_id, 500u128)), vec![]),
    );
    let minted: [u8; 32] = borsh::from_slice(ok(&r)).unwrap();
    assert_eq!(
        changes(&r),
        [
            change(
                0,
                minted,
                ChangeKind::Create {
                    capabilities: CAP_MINT
                }
            ),
            change(0, minted, ChangeKind::Issue { to: a, amount: 500 })
        ]
    );
    let r = w.run(&bob, call(issuer, "issue", enc(&(sub_id, 20u128)), vec![]));
    assert_eq!(
        changes(&r),
        [change(0, minted, ChangeKind::Issue { to: b, amount: 20 })]
    );
    let r = w.run(&bob, call(issuer, "issue", enc(&(sub_id, 0u128)), vec![]));
    assert_eq!(changes(&r), []);

    // asset_create, then each management function and a burn
    let sub_id = [2u8; 32];
    let meta = AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    };
    let capabilities = CAP_FREEZE | CAP_ALLOWLIST | CAP_PAUSE;
    let args = enc(&(sub_id, meta, capabilities, issuer, 1_000u128));
    let r = w.run(&alice, call(issuer, "create_asset", args, vec![]));
    let asset: [u8; 32] = borsh::from_slice(ok(&r)).unwrap();
    assert_eq!(
        changes(&r),
        [
            change(0, asset, ChangeKind::Create { capabilities }),
            change(
                0,
                asset,
                ChangeKind::Issue {
                    to: issuer,
                    amount: 1_000
                }
            )
        ]
    );
    let steps = [
        (
            "allow",
            enc(&(sub_id, b, true)),
            ChangeKind::Allow {
                account: b,
                allowed: true,
            },
        ),
        (
            "freeze",
            enc(&(sub_id, b, true)),
            ChangeKind::Freeze {
                account: b,
                frozen: true,
            },
        ),
        (
            "freeze",
            enc(&(sub_id, b, false)),
            ChangeKind::Freeze {
                account: b,
                frozen: false,
            },
        ),
        (
            "allow",
            enc(&(sub_id, b, false)),
            ChangeKind::Allow {
                account: b,
                allowed: false,
            },
        ),
        (
            "pause",
            enc(&(sub_id, true)),
            ChangeKind::Pause { paused: true },
        ),
        (
            "pause",
            enc(&(sub_id, false)),
            ChangeKind::Pause { paused: false },
        ),
        (
            "burn",
            enc(&(asset, 10u128)),
            ChangeKind::Burn {
                from: issuer,
                amount: 10,
            },
        ),
        // the asset never had `mint`: the renouncement names what it gave up
        (
            "renounce",
            enc(&(sub_id, CAP_FREEZE | CAP_MINT)),
            ChangeKind::Renounce {
                capabilities: CAP_FREEZE,
            },
        ),
    ];
    for (method, args, kind) in steps {
        let r = w.run(&alice, call(issuer, method, args, vec![]));
        assert_eq!(changes(&r), [change(0, asset, kind)], "{method}");
    }

    // a refused management call fails the op: nothing is recorded
    let r = w.run(
        &alice,
        call(issuer, "freeze", enc(&(sub_id, b, true)), vec![]),
    );
    failed(&r);
    assert!(r.asset_changes.is_empty(), "{r:?}");
}
