//! Host function parameters (SPEC §15): a fixed test contract calls host functions one after another as directed by
//! the input, with all parameters (pointers, lengths, heights) taken from the fuzz input.
//!
//! The input is the call argument `args`: the first byte mod 9 is the command count n; then n commands of 55 bytes,
//! each 1 byte selecting the host function (mod 25) plus 6 parameter slots of 9 bytes (1 byte type + 8-byte
//! little-endian value); then the data area. Parameter type mod 3: 0 = raw value, 1 = data area start + (value &
//! 0xFFFF) (a pointer near the input), 2 = value & 0x7FF (a small length or height).
//! `call_contract` / `try_call_contract` / `delegate_call` can call the contract itself (its ID and Borsh input placed
//! in the data area, optionally with attached funds, D-073); the recursion depth is limited by `MAX_CALL_DEPTH`.
//! The asset management functions (D-121) take their parameters the same way; `asset_create` has seven, so its
//! destination is its `sub_id` pointer (slot 0).
//!
//! Property: the host (`MockHost`) never reports a fault, so `vm.call` must return `Ok` (a `VmError` means an error
//! in the VM itself); executing the same input twice gives byte-identical results and host state; on success
//! `gas_used` does not exceed the limit.
//! With `KSANA_FUZZ_TRACE=1` set, the execution result of every input is printed (to check whether the seeds reach
//! the success paths).

#![no_main]

#[path = "../../crates/ksana-vm/tests/common/mod.rs"]
mod common;

use std::sync::OnceLock;

use common::{id, module, MockHost};
use ksana_primitives::DEVNET;
use ksana_vm::{CallOutcome, CallRequest, Vm};
use libfuzzer_sys::fuzz_target;

const GAS: u64 = 5_000_000;
/// ID of the test contract (`[1; 32]`).
const CONTRACT: u8 = 1;
/// Asset with a preset balance (`[9; 32]`), credited to the contract account so that `asset_transfer` /
/// `asset_balance` reach the success path.
const ASSET: u8 = 9;

const BODY: &str = r#"
  ;; value of the k-th parameter slot (types in the module docs)
  (func $arg (param $rec i32) (param $data i32) (param $k i32) (result i64)
    (local $at i32) (local $v i64) (local $tag i32)
    (local.set $at (i32.add (i32.add (local.get $rec) (i32.const 1)) (i32.mul (local.get $k) (i32.const 9))))
    (local.set $tag (i32.rem_u (i32.load8_u (local.get $at)) (i32.const 3)))
    (local.set $v (i64.load (i32.add (local.get $at) (i32.const 1))))
    (if (result i64) (i32.eqz (local.get $tag))
      (then (local.get $v))
      (else
        (if (result i64) (i32.eq (local.get $tag) (i32.const 1))
          (then (i64.extend_i32_u
            (i32.add (local.get $data) (i32.and (i32.wrap_i64 (local.get $v)) (i32.const 0xffff)))))
          (else (i64.and (local.get $v) (i64.const 0x7ff)))))))
  (func $a (param $rec i32) (param $data i32) (param $k i32) (result i32)
    (i32.wrap_i64 (call $arg (local.get $rec) (local.get $data) (local.get $k))))

  ;; execute one command
  (func $one (param $r i32) (param $d i32)
    (block $done
    (block $f24 (block $f23 (block $f22 (block $f21 (block $f20 (block $f19 (block $f18
    (block $f17 (block $f16 (block $f15 (block $f14 (block $f13 (block $f12 (block $f11 (block $f10 (block $f9
    (block $f8 (block $f7 (block $f6 (block $f5 (block $f4 (block $f3 (block $f2 (block $f1 (block $f0
      (br_table $f0 $f1 $f2 $f3 $f4 $f5 $f6 $f7 $f8 $f9 $f10 $f11 $f12 $f13 $f14 $f15 $f16 $f17
                $f18 $f19 $f20 $f21 $f22 $f23 $f24
        (i32.rem_u (i32.load8_u (local.get $r)) (i32.const 25))))
      (drop (call $storage_get (call $a (local.get $r) (local.get $d) (i32.const 0))
                               (call $a (local.get $r) (local.get $d) (i32.const 1))))
      (br $done))
      (call $storage_set (call $a (local.get $r) (local.get $d) (i32.const 0))
                         (call $a (local.get $r) (local.get $d) (i32.const 1))
                         (call $a (local.get $r) (local.get $d) (i32.const 2))
                         (call $a (local.get $r) (local.get $d) (i32.const 3)))
      (br $done))
      (call $storage_remove (call $a (local.get $r) (local.get $d) (i32.const 0))
                            (call $a (local.get $r) (local.get $d) (i32.const 1)))
      (br $done))
      (call $caller (call $a (local.get $r) (local.get $d) (i32.const 0)))
      (br $done))
      (call $self_id (call $a (local.get $r) (local.get $d) (i32.const 0)))
      (br $done))
      (drop (call $block_height))
      (br $done))
      (drop (call $btc_anchor_height))
      (br $done))
      (call $btc_header (call $arg (local.get $r) (local.get $d) (i32.const 0))
                        (call $a (local.get $r) (local.get $d) (i32.const 1)))
      (br $done))
      (call $asset_balance (call $a (local.get $r) (local.get $d) (i32.const 0))
                           (call $a (local.get $r) (local.get $d) (i32.const 1))
                           (call $a (local.get $r) (local.get $d) (i32.const 2)))
      (br $done))
      (call $asset_transfer (call $a (local.get $r) (local.get $d) (i32.const 0))
                            (call $a (local.get $r) (local.get $d) (i32.const 1))
                            (call $a (local.get $r) (local.get $d) (i32.const 2)))
      (br $done))
      (call $asset_issue (call $a (local.get $r) (local.get $d) (i32.const 0))
                         (call $a (local.get $r) (local.get $d) (i32.const 1))
                         (call $a (local.get $r) (local.get $d) (i32.const 2))
                         (call $a (local.get $r) (local.get $d) (i32.const 3))
                         (call $a (local.get $r) (local.get $d) (i32.const 4))
                         (call $a (local.get $r) (local.get $d) (i32.const 5)))
      (br $done))
      (drop (call $call_contract (call $a (local.get $r) (local.get $d) (i32.const 0))
                                 (call $a (local.get $r) (local.get $d) (i32.const 1))
                                 (call $a (local.get $r) (local.get $d) (i32.const 2))
                                 (call $a (local.get $r) (local.get $d) (i32.const 3))
                                 (call $a (local.get $r) (local.get $d) (i32.const 4))))
      (br $done))
      (call $emit_event (call $a (local.get $r) (local.get $d) (i32.const 0))
                        (call $a (local.get $r) (local.get $d) (i32.const 1)))
      (br $done))
      (call $sha256 (call $a (local.get $r) (local.get $d) (i32.const 0))
                    (call $a (local.get $r) (local.get $d) (i32.const 1))
                    (call $a (local.get $r) (local.get $d) (i32.const 2)))
      (br $done))
      (drop (call $verify_schnorr (call $a (local.get $r) (local.get $d) (i32.const 0))
                                  (call $a (local.get $r) (local.get $d) (i32.const 1))
                                  (call $a (local.get $r) (local.get $d) (i32.const 2))))
      (br $done))
      (drop (call $call_funds))
      (br $done))
      (drop (call $try_call_contract (call $a (local.get $r) (local.get $d) (i32.const 0))
                                     (call $a (local.get $r) (local.get $d) (i32.const 1))
                                     (call $a (local.get $r) (local.get $d) (i32.const 2))
                                     (call $a (local.get $r) (local.get $d) (i32.const 3))
                                     (call $a (local.get $r) (local.get $d) (i32.const 4))
                                     (call $arg (local.get $r) (local.get $d) (i32.const 5))))
      (br $done))
      (drop (call $delegate_call (call $a (local.get $r) (local.get $d) (i32.const 0))
                                 (call $a (local.get $r) (local.get $d) (i32.const 1))
                                 (call $a (local.get $r) (local.get $d) (i32.const 2))))
      (br $done))
      (call $asset_create (call $a (local.get $r) (local.get $d) (i32.const 0))
                          (call $a (local.get $r) (local.get $d) (i32.const 1))
                          (call $a (local.get $r) (local.get $d) (i32.const 2))
                          (call $a (local.get $r) (local.get $d) (i32.const 3))
                          (call $a (local.get $r) (local.get $d) (i32.const 4))
                          (call $a (local.get $r) (local.get $d) (i32.const 5))
                          (call $a (local.get $r) (local.get $d) (i32.const 0)))
      (br $done))
      (call $asset_freeze (call $a (local.get $r) (local.get $d) (i32.const 0))
                          (call $a (local.get $r) (local.get $d) (i32.const 1))
                          (call $a (local.get $r) (local.get $d) (i32.const 2)))
      (br $done))
      (call $asset_allow (call $a (local.get $r) (local.get $d) (i32.const 0))
                         (call $a (local.get $r) (local.get $d) (i32.const 1))
                         (call $a (local.get $r) (local.get $d) (i32.const 2)))
      (br $done))
      (call $asset_pause (call $a (local.get $r) (local.get $d) (i32.const 0))
                         (call $a (local.get $r) (local.get $d) (i32.const 1)))
      (br $done))
      (call $asset_renounce (call $a (local.get $r) (local.get $d) (i32.const 0))
                            (call $a (local.get $r) (local.get $d) (i32.const 1)))
      (br $done))
      (drop (call $asset_policy (call $a (local.get $r) (local.get $d) (i32.const 0))))
      (br $done))
      (drop (call $asset_status (call $a (local.get $r) (local.get $d) (i32.const 0))
                                (call $a (local.get $r) (local.get $d) (i32.const 1))))))

  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $base i32) (local $len i32) (local $n i32) (local $i i32) (local $data i32)
    (local.set $base (call $args_ptr (local.get $p)))
    (local.set $len (call $args_len (local.get $p)))
    (if (i32.eqz (local.get $len)) (then (return (call $ok (i32.const 0) (i32.const 0)))))
    (local.set $n (i32.rem_u (i32.load8_u (local.get $base)) (i32.const 9)))
    (if (i32.lt_u (local.get $len) (i32.add (i32.const 1) (i32.mul (local.get $n) (i32.const 55))))
      (then (return (call $ok (i32.const 0) (i32.const 0)))))
    (local.set $data (i32.add (i32.add (local.get $base) (i32.const 1)) (i32.mul (local.get $n) (i32.const 55))))
    (block $end
      (loop $next
        (br_if $end (i32.ge_u (local.get $i) (local.get $n)))
        (call $one (i32.add (i32.add (local.get $base) (i32.const 1)) (i32.mul (local.get $i) (i32.const 55)))
                   (local.get $data))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    ;; return the data area: the output packing and length check are exercised too
    (call $ok (local.get $data) (i32.sub (i32.add (local.get $base) (local.get $len)) (local.get $data))))
"#;

struct Fixture {
    vm: Vm,
    code: Vec<u8>,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let vm = Vm::new(&DEVNET).expect("the VM config is valid");
        let code = module(BODY);
        vm.validate_code(&code)
            .expect("the test contract passes the deployment check");
        Fixture { vm, code }
    })
}

fn run(args: &[u8]) -> (MockHost, Result<CallOutcome, ksana_vm::VmError>) {
    let f = fixture();
    let mut host = MockHost::default();
    host.deploy(id(CONTRACT), f.code.clone());
    host.height = 42;
    host.anchor = 300;
    for height in [0u32, 1, 100, 293, 294, 299, 300] {
        let mut header = [0u8; 80];
        header[..4].copy_from_slice(&height.to_le_bytes());
        host.headers.insert(height, header);
    }
    host.balances.insert((id(ASSET), id(CONTRACT)), 1_000_000);
    f.vm.call(
        host,
        CallRequest {
            contract: id(CONTRACT),
            caller: id(0xAA),
            method: "run".into(),
            args: args.to_vec(),
            funds: Vec::new(),
            allow_init: false,
        },
        GAS,
    )
}

fuzz_target!(|data: &[u8]| {
    let (first, r1) = run(data);
    let (second, r2) = run(data);
    assert_eq!(
        r1, r2,
        "two executions of the same input gave different results"
    );
    assert_eq!(first.storage, second.storage);
    assert_eq!(first.balances, second.balances);
    assert_eq!(first.assets, second.assets);
    assert_eq!(first.events, second.events);
    // Check whether the seeds reach the host functions' success paths: with KSANA_FUZZ_TRACE=1 the result of every
    // input is printed
    if std::env::var_os("KSANA_FUZZ_TRACE").is_some() {
        eprintln!("{r1:?}");
    }
    match r1.expect("the host never reports a fault; a VmError means an error in the VM itself") {
        CallOutcome::Success { gas_used, .. } => assert!(gas_used <= GAS),
        CallOutcome::Failure(_) => {}
    }
});
