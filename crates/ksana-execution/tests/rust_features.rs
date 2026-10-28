//! D-073: Rust contracts may use recursion, trait objects and `core::fmt` (the D-021 restriction is lifted); stack
//! depth is metered deterministically at runtime.

mod common;

use common::probe::*;
use common::*;
use ksana_execution::OpStatus;

#[test]
fn recursion_dynamic_dispatch_and_formatting_work() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);

    let r = w.run(&alice, call(p, "recurse", enc(&200u32), vec![]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(borsh::from_slice::<u32>(&r.output).unwrap(), 200);

    let r = w.run(&alice, call(p, "dyn_sum", enc(&10u32), vec![]));
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    // Even terms are added as is, odd terms doubled: 0+2+2+6+4+10+6+14+8+18
    assert_eq!(borsh::from_slice::<u64>(&r.output).unwrap(), 70);

    let r = w.run(
        &alice,
        call(p, "format", enc(&(255u64, "n".to_string())), vec![]),
    );
    assert_eq!(r.status, OpStatus::Success, "{r:?}");
    assert_eq!(
        borsh::from_slice::<String>(&r.output).unwrap(),
        "n=  255|ff"
    );
}

#[test]
fn deep_recursion_fails_deterministically() {
    let alice = user("alice");
    let mut w = World::new(&[&alice]);
    let p = w.deploy(&alice, vec![]);
    // Exceeding the stack budget: the op fails (trap), the fee is still charged, state unchanged
    let before = w.chain.ksa(&alice.account);
    let r = w.run(&alice, call(p, "recurse", enc(&1_000_000u32), vec![]));
    assert!(matches!(r.status, OpStatus::Failed(_)), "{r:?}");
    assert_eq!(w.chain.ksa(&alice.account), before - FEE);
    // Replaying the same op on another chain gives the same result
    let mut again = World::new(&[&alice]);
    let q = again.deploy(&alice, vec![]);
    assert_eq!(p, q);
    let r2 = again.run(&alice, call(q, "recurse", enc(&1_000_000u32), vec![]));
    assert_eq!(r.status, r2.status);
    assert_eq!(r.gas_used, r2.gas_used);
}
