//! Catchable sub-calls and delegate calls (D-073): `try_call_contract` rolls back only the failed layer (storage,
//! transfers and events) and the caller continues; the caller's own errors cannot be caught; the callee may use at most
//! 63/64 of the remaining gas. `delegate_call` runs another piece of code in the current contract's context.

mod common;

use common::probe::*;
use common::*;
use ksana_execution::OpStatus;
use ksana_primitives::Funds;

fn try_forward(
    target: [u8; 32],
    method: &str,
    inner: Vec<u8>,
    funds: Vec<Funds>,
    gas: u64,
) -> Vec<u8> {
    enc(&(target, method.to_string(), inner, funds, gas))
}

fn status(output: &[u8]) -> (u8, Vec<u8>) {
    borsh::from_slice(output).unwrap()
}

fn get(w: &mut World, u: &User, contract: [u8; 32], key: &[u8]) -> Option<Vec<u8>> {
    let r = w.run(u, call(contract, "get", enc(&key.to_vec()), vec![]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    borsh::from_slice(&r.output).unwrap()
}

#[test]
fn try_call_rolls_back_only_the_failed_layer() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let a = w.deploy(&alice, vec![]);
    let b = w.deploy(&alice, vec![]);
    let c = w.deploy(&alice, vec![]);

    // Success: same as call, b's writes and the attached funds are kept
    let set = enc(&(b"k".to_vec(), b"v".to_vec()));
    let r = w.run(
        &alice,
        call(
            a,
            "try_forward",
            try_forward(b, "set", set, vec![ksa(5)], 0),
            vec![ksa(50)],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(status(&r.output), (0, enc(&())));
    assert_eq!(get(&mut w, &alice, b, b"k"), Some(b"v".to_vec()));
    assert_eq!((w.chain.ksa(&a), w.chain.ksa(&b)), (45, 5));

    // revert: b's layer is rolled back (including the attached funds), a receives the revert data and continues, its
    // own writes and events are kept
    let r = w.run(
        &alice,
        call(
            a,
            "try_forward",
            try_forward(b, "revert", vec![], vec![ksa(5)], 0),
            vec![ksa(10)],
        ),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(status(&r.output), (1, b"probe revert".to_vec()));
    assert_eq!((w.chain.ksa(&a), w.chain.ksa(&b)), (55, 5));
    assert_eq!(get(&mut w, &alice, a, b"after"), Some(b"1".to_vec()));
    assert_eq!(r.events.len(), 1);
    assert_eq!(
        (r.events[0].contract, r.events[0].data.clone()),
        (a, b"after".to_vec())
    );

    // Nested: b calls c (c writes storage, emits an event and receives b's attached funds) and then reverts: the b and
    // c layers are rolled back together
    let set2 = enc(&(b"k2".to_vec(), b"v2".to_vec()));
    let inner = enc(&(c, "set".to_string(), set2, vec![ksa(3)]));
    let r = w.run(
        &alice,
        call(
            a,
            "try_forward",
            try_forward(b, "forward_then_revert", inner, vec![], 0),
            vec![],
        ),
    );
    assert_eq!(status(&r.output), (1, b"probe revert".to_vec()));
    assert_eq!(get(&mut w, &alice, c, b"k2"), None);
    assert_eq!((w.chain.ksa(&b), w.chain.ksa(&c)), (5, 0));
    assert_eq!(r.events.len(), 1, "only a's event remains: {:?}", r.events);

    // trap, out of gas, missing contract, insufficient balance for attached funds: failure (status 2, empty data), the
    // caller continues
    for (target, method, funds, gas) in [
        (b, "trap", vec![], 0),
        (b, "spin", vec![], 200_000),
        ([0x77; 32], "whoami", vec![], 0),
        (b, "whoami", vec![ksa(1_000)], 0),
    ] {
        let r = w.run(
            &alice,
            call(
                a,
                "try_forward",
                try_forward(target, method, vec![], funds, gas),
                vec![],
            ),
        );
        assert_eq!(r.status, OpStatus::Success, "{method}: {r:?}");
        assert_eq!(status(&r.output), (2, Vec::new()), "{method}");
    }
    assert_eq!((w.chain.ksa(&a), w.chain.ksa(&b)), (55, 5));
}

#[test]
fn callee_gets_at_most_63_64_of_the_remaining_gas() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let a = w.deploy(&alice, vec![]);
    let b = w.deploy(&alice, vec![]);
    // An infinite loop without a limit (or with a limit above the remainder) uses up 63/64; the caller writes storage
    // and returns with the remaining 1/64
    for gas in [0, u64::MAX] {
        let r = w.run_gas(
            &alice,
            2_000_000,
            call(
                a,
                "try_forward",
                try_forward(b, "spin", vec![], vec![], gas),
                vec![],
            ),
        );
        assert_eq!(r.status, OpStatus::Success, "{r:?}");
        assert_eq!(status(&r.output), (2, Vec::new()));
        assert!(
            r.gas_used > 1_950_000 && r.gas_used <= 2_000_000,
            "{}",
            r.gas_used
        );
    }
}

#[test]
fn caller_errors_are_not_catchable() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let a = w.deploy(&alice, vec![]);
    let b = w.deploy(&alice, vec![]);
    let unsorted = vec![
        Funds {
            asset: [9; 32],
            amount: 1,
        },
        ksa(1),
    ];
    for (method, funds) in [
        ("init", vec![]),
        ("whoami", unsorted),
        ("whoami", vec![ksa(0)]),
    ] {
        let r = w.run(
            &alice,
            call(
                a,
                "try_forward",
                try_forward(b, method, vec![], funds, 0),
                vec![],
            ),
        );
        assert!(matches!(r.status, OpStatus::Failed(_)), "{method}: {r:?}");
        assert_eq!(get(&mut w, &alice, a, b"after"), None);
    }
}

#[test]
fn delegate_call_uses_the_callers_storage_balance_and_context() {
    let alice = user("alice");
    let bob = user("bob");
    let mut w = World::new(&[&alice, &bob]);
    let a = w.deploy(&alice, vec![]);
    let b = w.deploy(&alice, vec![]);
    let delegate = |method: &str, inner: Vec<u8>| enc(&(b, method.to_string(), inner));

    // Storage is written under a
    let set = enc(&(b"k".to_vec(), b"v".to_vec()));
    let r = w.run(&alice, call(a, "delegate", delegate("set", set), vec![]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(get(&mut w, &alice, a, b"k"), Some(b"v".to_vec()));
    assert_eq!(get(&mut w, &alice, b, b"k"), None);
    assert_eq!(r.events[0].contract, a, "events are recorded under a too");

    // caller, self_id and attached funds carry over from the current call; the attached funds are transferred only once
    let r = w.run(
        &bob,
        call(a, "delegate", delegate("whoami", vec![]), vec![]),
    );
    let (caller, me): ([u8; 32], [u8; 32]) = borsh::from_slice(&r.output).unwrap();
    assert_eq!((caller, me), (bob.account, a));
    let r = w.run(
        &bob,
        call(a, "delegate", delegate("funds", vec![]), vec![ksa(4)]),
    );
    let (caller, funds): ([u8; 32], Vec<Funds>) = borsh::from_slice(&r.output).unwrap();
    assert_eq!((caller, funds), (bob.account, vec![ksa(4)]));
    assert_eq!((w.chain.ksa(&a), w.chain.ksa(&b)), (4, 0));

    // The delegated code sends out a's balance
    let send = enc(&(KSA, alice.account, 4u128));
    let before = w.chain.ksa(&alice.account);
    let r = w.run(&bob, call(a, "delegate", delegate("send", send), vec![]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(w.chain.ksa(&a), 0);
    assert_eq!(w.chain.ksa(&alice.account), before + 4);

    // Failure: the whole op fails
    let r = w.run(
        &bob,
        call(a, "delegate", delegate("revert", vec![]), vec![]),
    );
    assert!(matches!(r.status, OpStatus::Failed(_)), "{r:?}");
}
