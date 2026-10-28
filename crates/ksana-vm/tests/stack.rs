//! Deterministic runtime stack metering (D-073, ABI section 5): recursion and indirect calls can be deployed, and the
//! estimated stack usage exceeding `STACK_BUDGET` traps deterministically, independent of the engine configuration;
//! the instrumentation does not change program semantics (`return`, `br` to the function label, multi-value returns,
//! existing globals).

mod common;

use common::{id, module, MockHost};
use ksana_primitives::DEVNET;
use ksana_vm::{CallOutcome, CallRequest, FailureReason, Vm};

const GAS: u64 = 1_000_000_000;

fn run(vm: &Vm, body: &str, args: &[u8]) -> Result<CallOutcome, ksana_vm::VmError> {
    let mut host = MockHost::default();
    host.deploy(id(1), module(body));
    vm.call(
        host,
        CallRequest {
            contract: id(1),
            caller: id(0xAA),
            method: "x".into(),
            args: args.to_vec(),
            funds: Vec::new(),
            allow_init: false,
        },
        GAS,
    )
    .1
}

/// Returns (output, gas) on success; the reason on failure.
fn outcome(r: Result<CallOutcome, ksana_vm::VmError>) -> Result<(Vec<u8>, u64), FailureReason> {
    match r.expect("no VM internal error expected") {
        CallOutcome::Success { output, gas_used } => Ok((output, gas_used)),
        CallOutcome::Failure(reason) => Err(reason),
    }
}

fn is_metering_trap(reason: &FailureReason) -> bool {
    // the `unreachable` inserted by the metering, not wasmtime's native stack overflow
    matches!(reason, FailureReason::Trap(msg) if msg.contains("unreachable"))
}

/// args is a little-endian u32 n: returns the u32 result of `$r(n)` (direct recursion n levels deep).
const RECURSE: &str = r#"
  (func $r (param $n i32) (result i32)
    (if (result i32) (i32.eqz (local.get $n))
      (then (i32.const 0))
      (else (i32.add (call $r (i32.sub (local.get $n) (i32.const 1))) (i32.const 1)))))
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (i32.store (i32.const 300) (call $r (i32.load (call $args_ptr (local.get $p)))))
    (call $ok (i32.const 300) (i32.const 4)))
"#;

fn depth_ok(vm: &Vm, body: &str, n: u32) -> Result<u64, FailureReason> {
    outcome(run(vm, body, &n.to_le_bytes())).map(|(_, gas)| gas)
}

/// Binary search for the maximum recursion depth that succeeds.
fn max_depth(vm: &Vm, body: &str) -> u32 {
    let (mut lo, mut hi) = (0u32, 100_000u32);
    assert!(depth_ok(vm, body, lo).is_ok());
    let err = depth_ok(vm, body, hi).unwrap_err();
    assert!(is_metering_trap(&err), "{err:?}");
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match depth_ok(vm, body, mid) {
            Ok(_) => lo = mid,
            Err(e) => {
                assert!(is_metering_trap(&e), "{e:?}");
                hi = mid;
            }
        }
    }
    lo
}

#[test]
fn recursion_is_metered_deterministically() {
    let a = Vm::with_parallel_compilation(&DEVNET, true).unwrap();
    let b = Vm::with_parallel_compilation(&DEVNET, false).unwrap();
    let depth = max_depth(&a, RECURSE);
    // $r's frame: 64 + 16 × (1 parameter + maximum operand stack height 2) = 112 bytes; the entry function takes
    // part of the budget as well
    assert!(
        (2_300..=2_340).contains(&depth),
        "maximum recursion depth {depth} does not match the budget 262 144 / 112"
    );
    // both engine configurations agree at the boundary, and the gas agrees too
    assert_eq!(depth_ok(&a, RECURSE, depth), depth_ok(&b, RECURSE, depth));
    assert_eq!(
        depth_ok(&a, RECURSE, depth + 1),
        depth_ok(&b, RECURSE, depth + 1)
    );
    let out = outcome(run(&a, RECURSE, &depth.to_le_bytes())).unwrap().0;
    assert_eq!(out, depth.to_le_bytes(), "recursion result is correct");
}

#[test]
fn mutual_and_indirect_recursion_are_metered() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mutual = r#"
      (func $a (param $n i32) (result i32)
        (if (result i32) (i32.eqz (local.get $n)) (then (i32.const 0))
          (else (call $b (i32.sub (local.get $n) (i32.const 1))))))
      (func $b (param $n i32) (result i32) (call $a (local.get $n)))
      (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
        (drop (call $a (i32.load (call $args_ptr (local.get $p)))))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    let indirect = r#"
      (type $unary (func (param i32) (result i32)))
      (table 1 1 funcref)
      (elem (i32.const 0) $f)
      (func $f (type $unary) (param $n i32) (result i32)
        (if (result i32) (i32.eqz (local.get $n)) (then (i32.const 0))
          (else (call_indirect (type $unary) (i32.sub (local.get $n) (i32.const 1)) (i32.const 0)))))
      (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
        (drop (call $f (i32.load (call $args_ptr (local.get $p)))))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    for body in [mutual, indirect] {
        assert!(depth_ok(&vm, body, 500).is_ok());
        let err = depth_ok(&vm, body, 1_000_000).unwrap_err();
        assert!(is_metering_trap(&err), "{err:?}");
    }
}

#[test]
fn a_frame_over_the_budget_traps_on_entry() {
    let vm = Vm::new(&DEVNET).unwrap();
    let body = format!(
        r#"(func $big {}
             (local.set 0 (i64.const 1)))
           (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
             (if (i32.load8_u (call $args_ptr (local.get $p))) (then (call $big)))
             (call $ok (i32.const 0) (i32.const 0)))"#,
        "(local i64)".repeat(20_000)
    );
    assert!(
        depth_ok(&vm, &body, 0).is_ok(),
        "no trap without calling it"
    );
    let err = depth_ok(&vm, &body, 1).unwrap_err();
    assert!(is_metering_trap(&err), "{err:?}");
}

#[test]
fn every_exit_path_releases_its_frame() {
    // 100,000 calls, each returning through a different exit: if any exit failed to subtract its frame, the counter
    // would quickly exceed the budget
    let body = r#"
      (global $g (mut i32) (i32.const 7))
      ;; exits: an explicit return inside a nested block (m = 1), a br to the function label (m = 2), a br_table whose
      ;; default target is the function label (m = 3), the block result falling through to the function end (m = 0)
      (func $exit (param $i i32) (result i32)
        (local $m i32)
        (local.set $m (i32.rem_u (local.get $i) (i32.const 4)))
        (block $b
          (br_if $b (i32.ge_u (local.get $m) (i32.const 2)))
          (if (i32.eq (local.get $m) (i32.const 1)) (then (return (i32.const 1)))))
        (block $a (result i32)
          (if (i32.eq (local.get $m) (i32.const 2)) (then (br 2 (i32.const 2))))
          (br_table $a 1 (i32.add (local.get $m) (i32.const 100)) (local.get $m))))
      ;; multi-value return
      (func $pair (param $i i32) (result i32 i32)
        (if (i32.and (local.get $i) (i32.const 1))
          (then (return (local.get $i) (i32.const 1))))
        (local.get $i) (i32.const 0))
      (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
        (local $i i32) (local $sum i32)
        (block $done
          (loop $next
            (br_if $done (i32.ge_u (local.get $i) (i32.const 100000)))
            (local.set $sum (i32.add (local.get $sum) (call $exit (local.get $i))))
            (local.set $sum (i32.add (local.get $sum) (i32.add (call $pair (local.get $i)))))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $next)))
        ;; the contract's own globals are unaffected
        (global.set $g (i32.add (global.get $g) (i32.const 1)))
        (i32.store (i32.const 300) (local.get $sum))
        (i32.store (i32.const 304) (global.get $g))
        (call $ok (i32.const 300) (i32.const 8)))"#;
    let vm = Vm::new(&DEVNET).unwrap();
    let (out, _) = outcome(run(&vm, body, b"")).unwrap();
    // $exit: i % 4 of 0 → 100, 1 → 1, 2 → 2, 3 → 103; $pair: i + (i & 1).
    let mut expected: u32 = 0;
    for i in 0u32..100_000 {
        expected = expected.wrapping_add(match i % 4 {
            0 => 100,
            1 => 1,
            2 => 2,
            _ => 103,
        });
        expected = expected.wrapping_add(i + (i & 1));
    }
    assert_eq!(&out[..4], expected.to_le_bytes());
    assert_eq!(&out[4..], 8u32.to_le_bytes());
}
