//! Contract calls: the calling convention, gas, host functions, nested calls and various failures (docs/ABI.md
//! sections 2–4).

mod common;

use common::{id, module, MockHost};
use ksana_primitives::hash::sha256;
use ksana_primitives::DEVNET;
use ksana_vm::{
    gas, CallOutcome, CallRequest, FailureReason, NativeOutput, NativeTxInfo, Vm, VmError,
};

const GAS: u64 = 10_000_000;

fn call(
    vm: &Vm,
    host: MockHost,
    contract: u8,
    method: &str,
    args: &[u8],
    gas: u64,
) -> (MockHost, Result<CallOutcome, VmError>) {
    vm.call(
        host,
        CallRequest {
            contract: id(contract),
            caller: id(0xAA),
            method: method.into(),
            args: args.to_vec(),
            funds: Vec::new(),
            allow_init: false,
        },
        gas,
    )
}

fn host_with(contracts: &[(u8, &str)]) -> MockHost {
    let mut host = MockHost::default();
    for (i, body) in contracts {
        host.deploy(id(*i), module(body));
    }
    host
}

fn success(r: Result<CallOutcome, VmError>) -> (Vec<u8>, u64) {
    match r {
        Ok(CallOutcome::Success { output, gas_used }) => (output, gas_used),
        other => panic!("should succeed, got {other:?}"),
    }
}

fn failure(r: Result<CallOutcome, VmError>) -> FailureReason {
    match r {
        Ok(CallOutcome::Failure(reason)) => reason,
        other => panic!("should fail, got {other:?}"),
    }
}

const ECHO: &str = r#"
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (call $ok (call $args_ptr (local.get $p)) (call $args_len (local.get $p))))
"#;

#[test]
fn echo_returns_args_and_charges_gas() {
    let vm = Vm::new(&DEVNET).unwrap();
    let (_, r) = call(&vm, host_with(&[(1, ECHO)]), 1, "echo", b"hello", GAS);
    let (output, gas_used) = success(r);
    assert_eq!(output, b"hello");
    assert!(gas_used > 0 && gas_used < GAS);
}

#[test]
fn explicit_revert_and_trap_and_out_of_gas() {
    let vm = Vm::new(&DEVNET).unwrap();
    let revert = r#"
      (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
        (call $err (call $args_ptr (local.get $p)) (call $args_len (local.get $p))))"#;
    let (_, r) = call(&vm, host_with(&[(1, revert)]), 1, "x", b"bad input", GAS);
    assert_eq!(failure(r), FailureReason::Revert(b"bad input".to_vec()));

    let trap = r#"(func (export "ksana_call") (param i32 i32) (result i64) unreachable)"#;
    let (_, r) = call(&vm, host_with(&[(1, trap)]), 1, "x", b"", GAS);
    assert!(matches!(failure(r), FailureReason::Trap(_)));

    let spin = r#"(func (export "ksana_call") (param i32 i32) (result i64) (loop $l (br $l)) (i64.const 0))"#;
    let (_, r) = call(&vm, host_with(&[(1, spin)]), 1, "x", b"", GAS);
    assert_eq!(failure(r), FailureReason::OutOfGas);
}

#[test]
fn invalid_output_is_rejected() {
    let vm = Vm::new(&DEVNET).unwrap();
    // returns a pointer to the 3 bytes 0x02 0x00 0x00: not a valid Result encoding
    let bad = r#"
      (data (i32.const 100) "\02\00\00")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (i64.or (i64.shl (i64.const 100) (i64.const 32)) (i64.const 3)))"#;
    let (_, r) = call(&vm, host_with(&[(1, bad)]), 1, "x", b"", GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
    // returns an out-of-bounds pointer
    let oob = r#"(func (export "ksana_call") (param i32 i32) (result i64) (i64.const -1))"#;
    let (_, r) = call(&vm, host_with(&[(1, oob)]), 1, "x", b"", GAS);
    assert!(matches!(
        failure(r),
        FailureReason::Rejected(_) | FailureReason::Trap(_)
    ));
}

#[test]
fn init_is_reserved_and_missing_contract_fails() {
    let vm = Vm::new(&DEVNET).unwrap();
    let (_, r) = call(&vm, host_with(&[(1, ECHO)]), 1, "init", b"", GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
    let (_, r) = call(&vm, MockHost::default(), 9, "echo", b"", GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
    // calling init is allowed at deployment
    let (host, _) = (host_with(&[(1, ECHO)]), ());
    let (_, r) = vm.call(
        host,
        CallRequest {
            contract: id(1),
            caller: id(0xAA),
            method: "init".into(),
            args: b"x".to_vec(),
            funds: Vec::new(),
            allow_init: true,
        },
        GAS,
    );
    assert_eq!(success(r).0, b"x");
}

/// Writes the value "v" under args as the key, then reads it back and returns it.
const STORE: &str = r#"
  (data (i32.const 200) "v")
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $packed i64)
    (call $storage_set (call $args_ptr (local.get $p)) (call $args_len (local.get $p)) (i32.const 200) (i32.const 1))
    (local.set $packed (call $storage_get (call $args_ptr (local.get $p)) (call $args_len (local.get $p))))
    (call $ok (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))) (i32.wrap_i64 (local.get $packed))))
"#;

#[test]
fn storage_roundtrip_and_isolation_between_contracts() {
    let vm = Vm::new(&DEVNET).unwrap();
    let (host, r) = call(&vm, host_with(&[(1, STORE)]), 1, "x", b"key", GAS);
    assert_eq!(success(r).0, b"v");
    assert_eq!(
        host.storage.get(&(id(1), b"key".to_vec())),
        Some(&b"v".to_vec())
    );
    assert_eq!(host.storage.len(), 1);
}

#[test]
fn storage_key_limit_and_gas_charging() {
    let vm = Vm::new(&DEVNET).unwrap();
    let long_key = vec![7u8; 257];
    let (_, r) = call(&vm, host_with(&[(1, STORE)]), 1, "x", &long_key, GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
    // storage_set costs at least 10,000 gas: giving only 5,000 necessarily runs out of gas
    let (_, r) = call(&vm, host_with(&[(1, STORE)]), 1, "x", b"k", 5_000);
    assert_eq!(failure(r), FailureReason::OutOfGas);
}

#[test]
fn missing_storage_key_returns_minus_one() {
    let vm = Vm::new(&DEVNET).unwrap();
    let probe = r#"
      (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
        (if (i64.ne (call $storage_get (i32.const 0) (i32.const 1)) (i64.const -1))
          (then unreachable))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    let (_, r) = call(&vm, host_with(&[(1, probe)]), 1, "x", b"", GAS);
    success(r);
}

#[test]
fn fatal_host_error_aborts_instead_of_failing() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut host = host_with(&[(1, STORE)]);
    host.fail_fatal = true;
    let (_, r) = call(&vm, host, 1, "x", b"k", GAS);
    assert!(matches!(r, Err(VmError::Host(_))));
}

#[test]
fn btc_header_depth_rule() {
    let vm = Vm::new(&DEVNET).unwrap();
    // reads the block header at the height given in args (little-endian u32) and returns it
    let reader = r#"
      (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
        (call $btc_header (i64.extend_i32_u (i32.load (call $args_ptr (local.get $p)))) (i32.const 300))
        (call $ok (i32.const 300) (i32.const 80)))"#;
    let mut host = host_with(&[(1, reader)]);
    host.anchor = 110;
    for h in 100..=110u32 {
        host.headers.insert(h, [h as u8; 80]);
    }
    // anchor − BTC_QUERY_DEPTH = 104
    let (host, r) = call(&vm, host, 1, "x", &104u32.to_le_bytes(), GAS);
    assert_eq!(success(r).0, vec![104u8; 80]);
    let (host, r) = call(&vm, host, 1, "x", &105u32.to_le_bytes(), GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
    let (_, r) = call(&vm, host, 1, "x", &50u32.to_le_bytes(), GAS);
    assert!(
        matches!(failure(r), FailureReason::Rejected(_)),
        "unknown height"
    );
}

/// Calls the contract given by the first 32 bytes of args, with the remaining bytes as the input; returns the callee's
/// result.
const PROXY: &str = r#"
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32) (local $packed i64)
    (local.set $a (call $args_ptr (local.get $p)))
    (local.set $packed
      (call $call_contract (local.get $a) (i32.add (local.get $a) (i32.const 32))
        (i32.sub (call $args_len (local.get $p)) (i32.const 32)) (i32.const 0) (i32.const 0)))
    (call $ok (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))) (i32.wrap_i64 (local.get $packed))))
"#;

/// Returns caller(32) ‖ self_id(32).
const WHO: &str = r#"
  (func (export "ksana_call") (param i32 i32) (result i64)
    (call $caller (i32.const 400))
    (call $self_id (i32.const 432))
    (call $ok (i32.const 400) (i32.const 64)))
"#;

fn borsh_input(method: &str, args: &[u8]) -> Vec<u8> {
    borsh::to_vec(&(method.to_string(), args.to_vec())).unwrap()
}

#[test]
fn nested_call_sees_caller_and_self() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut args = id(2).to_vec();
    args.extend(borsh_input("who", b""));
    let (_, r) = call(&vm, host_with(&[(1, PROXY), (2, WHO)]), 1, "go", &args, GAS);
    let out = success(r).0;
    assert_eq!(
        &out[..32],
        &id(1),
        "the caller seen by the callee is the contract one level up"
    );
    assert_eq!(&out[32..], &id(2));
    // the top-level caller is the signing account
    let (_, r) = call(&vm, host_with(&[(2, WHO)]), 2, "who", b"", GAS);
    assert_eq!(&success(r).0[..32], &id(0xAA));
}

#[test]
fn nested_revert_fails_the_whole_call_and_init_is_not_callable() {
    let vm = Vm::new(&DEVNET).unwrap();
    let reverter = r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $err (i32.const 0) (i32.const 1)))"#;
    let mut args = id(2).to_vec();
    args.extend(borsh_input("x", b""));
    let (_, r) = call(
        &vm,
        host_with(&[(1, PROXY), (2, reverter)]),
        1,
        "go",
        &args,
        GAS,
    );
    assert!(matches!(failure(r), FailureReason::Revert(_)));

    let mut args = id(2).to_vec();
    args.extend(borsh_input("init", b""));
    let (_, r) = call(&vm, host_with(&[(1, PROXY), (2, WHO)]), 1, "go", &args, GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
}

#[test]
fn call_depth_limit() {
    let vm = Vm::new(&DEVNET).unwrap();
    // contract 1 calls itself, passing args down unchanged: unbounded re-entry, must be rejected at level 8
    let mut args = id(1).to_vec();
    let inner = {
        let mut a = id(1).to_vec();
        a.extend([0u8; 0]);
        a
    };
    let _ = inner;
    // builds a self-referential input: input = borsh(("go", args)), args = id(1) ‖ input; 12 levels by recurrence
    let mut payload = borsh_input("go", &id(1));
    for _ in 0..12 {
        let mut a = id(1).to_vec();
        a.extend(&payload);
        payload = borsh_input("go", &a);
    }
    args.extend(&payload);
    let (_, r) = call(&vm, host_with(&[(1, PROXY)]), 1, "go", &args, GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));

    // exactly 8 levels (top level + 7 nested, the innermost calling WHO) can succeed
    let mut payload = borsh_input("who", b"");
    let mut target = 2u8;
    for _ in 0..6 {
        let mut a = id(target).to_vec();
        a.extend(&payload);
        payload = borsh_input("go", &a);
        target = 1;
    }
    let mut args = id(target).to_vec();
    args.extend(&payload);
    let (_, r) = call(&vm, host_with(&[(1, PROXY), (2, WHO)]), 1, "go", &args, GAS);
    success(r);
}

#[test]
fn instance_limit_per_operation() {
    let vm = Vm::new(&DEVNET).unwrap();
    // calls contract 2 100 times in a loop: exceeds the limit of 64 instances per op
    let looper = r#"
      (data (i32.const 500) "\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02")
      (data (i32.const 540) "\03\00\00\00who\00\00\00\00")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (local $i i32)
        (loop $l
          (drop (call $call_contract (i32.const 500) (i32.const 540) (i32.const 11) (i32.const 0) (i32.const 0)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br_if $l (i32.lt_u (local.get $i) (i32.const 100))))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    let (_, r) = call(
        &vm,
        host_with(&[(1, looper), (2, WHO)]),
        1,
        "go",
        b"",
        100_000_000,
    );
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
}

#[test]
fn memory_growth_is_capped() {
    let vm = Vm::new(&DEVNET).unwrap();
    // requests growing by 300 pages (over the 16 MiB limit): memory.grow returns -1 instead of trapping
    let grower = r#"
      (func (export "ksana_call") (param i32 i32) (result i64)
        (if (i32.ne (memory.grow (i32.const 300)) (i32.const -1)) (then unreachable))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    let (_, r) = call(&vm, host_with(&[(1, grower)]), 1, "x", b"", GAS);
    success(r);
}

#[test]
fn assets_issue_and_transfer_from_self() {
    let vm = Vm::new(&DEVNET).unwrap();
    // issues 1000 of this contract's asset to itself (sub_id all 7s, meta = borsh{name:"T", symbol:"T", decimals:0}),
    // then transfers 400 to 0xBB
    let issuer = r#"
      (data (i32.const 600) "\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07\07")
      (data (i32.const 640) "\01\00\00\00T\01\00\00\00T\00")
      (data (i32.const 660) "\e8\03\00\00\00\00\00\00\00\00\00\00\00\00\00\00")
      (data (i32.const 680) "\90\01\00\00\00\00\00\00\00\00\00\00\00\00\00\00")
      (data (i32.const 700) "\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb\bb")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (call $self_id (i32.const 800))
        (call $asset_issue (i32.const 600) (i32.const 640) (i32.const 11) (i32.const 800) (i32.const 660) (i32.const 900))
        (call $asset_transfer (i32.const 900) (i32.const 700) (i32.const 680))
        (call $ok (i32.const 900) (i32.const 32)))"#;
    let (host, r) = call(&vm, host_with(&[(1, issuer)]), 1, "x", b"", GAS);
    let asset_bytes = success(r).0;
    let mut preimage = id(1).to_vec();
    preimage.extend([7u8; 32]);
    let asset = sha256(&preimage);
    assert_eq!(
        asset_bytes,
        asset.to_vec(),
        "asset ID = sha256(issuing contract ‖ sub_id)"
    );
    assert_eq!(host.balances.get(&(asset, id(1))), Some(&600));
    assert_eq!(host.balances.get(&(asset, id(0xBB))), Some(&400));
    assert_eq!(host.assets.get(&asset).map(|a| a.1), Some(1000));
}

#[test]
fn transfer_without_balance_fails() {
    let vm = Vm::new(&DEVNET).unwrap();
    let spender = r#"
      (data (i32.const 680) "\01\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (call $asset_transfer (i32.const 0) (i32.const 700) (i32.const 680))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    let (_, r) = call(&vm, host_with(&[(1, spender)]), 1, "x", b"", GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
}

#[test]
fn events_sha256_and_schnorr() {
    let vm = Vm::new(&DEVNET).unwrap();
    let crypto = r#"
      (data (i32.const 100) "abc")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (call $emit_event (i32.const 100) (i32.const 3))
        (call $sha256 (i32.const 100) (i32.const 3) (i32.const 200))
        ;; all-zero public key / message / signature: necessarily invalid, returns 0
        (i32.store8 (i32.const 232) (call $verify_schnorr (i32.const 300) (i32.const 300) (i32.const 300)))
        (call $ok (i32.const 200) (i32.const 33)))"#;
    let (host, r) = call(&vm, host_with(&[(1, crypto)]), 1, "x", b"", GAS);
    let out = success(r).0;
    assert_eq!(&out[..32], &sha256(b"abc"));
    assert_eq!(out[32], 0);
    assert_eq!(host.events, vec![(id(1), b"abc".to_vec())]);
}

#[test]
fn gas_is_deterministic_across_engines() {
    // the same call consumes exactly the same gas on engines with parallel and serial compilation
    let a = Vm::with_parallel_compilation(&DEVNET, true).unwrap();
    let b = Vm::with_parallel_compilation(&DEVNET, false).unwrap();
    let (_, ra) = call(&a, host_with(&[(1, STORE)]), 1, "x", b"key", GAS);
    let (_, rb) = call(&b, host_with(&[(1, STORE)]), 1, "x", b"key", GAS);
    assert_eq!(success(ra), success(rb));
}

#[test]
fn gas_limit_is_exact_even_after_the_last_fuel_check() {
    // wasmtime checks fuel only at function entry and loop headers: the straight-line code after `$ok` returns is not
    // checked again. Wherever the excess occurs, a call whose consumption exceeds the limit must fail (D-020).
    let tail: String = (0..40)
        .map(|i| format!("(drop (i32.add (i32.const {i}) (i32.const 1)))\n"))
        .collect();
    let body = format!(
        r#"(func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
             (local $r i64)
             (local.set $r (call $ok (i32.const 0) (i32.const 0)))
             {tail}
             (local.get $r))"#
    );
    let vm = Vm::new(&DEVNET).unwrap();
    let host = host_with(&[(1, body.as_str())]);
    let (host, r) = call(&vm, host, 1, "x", b"", GAS);
    let (_, exact) = success(r);
    // the trailing 40 `i32.const + i32.add` (drop is 0) total 80 fuel, all after the last check
    let mut host = host;
    for limit in exact - 80..exact {
        let (h, r) = call(&vm, host, 1, "x", b"", limit);
        assert_eq!(failure(r), FailureReason::OutOfGas, "limit {limit}");
        host = h;
    }
    let (host, r) = call(&vm, host, 1, "x", b"", exact);
    assert_eq!(success(r).1, exact);
    let (_, r) = call(&vm, host, 1, "x", b"", exact + 1);
    assert_eq!(success(r).1, exact);
}

/// A proxy with attached funds (D-073): args = callee contract ID(32) ‖ funds length u32 ‖ funds ‖ input;
/// calls `call_contract(callee, input, funds)` and returns the callee's result.
const FUNDS_PROXY: &str = r#"
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32) (local $fl i32) (local $packed i64)
    (local.set $a (call $args_ptr (local.get $p)))
    (local.set $fl (i32.load (i32.add (local.get $a) (i32.const 32))))
    (local.set $packed
      (call $call_contract
        (local.get $a)
        (i32.add (i32.add (local.get $a) (i32.const 36)) (local.get $fl))
        (i32.sub (i32.sub (call $args_len (local.get $p)) (i32.const 36)) (local.get $fl))
        (i32.add (local.get $a) (i32.const 36))
        (local.get $fl)))
    (call $ok (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))) (i32.wrap_i64 (local.get $packed))))
"#;

/// Returns the result of `call_funds` (Borsh-encoded `Vec<Funds>`).
const FUNDS_READER: &str = r#"
  (func (export "ksana_call") (param i32 i32) (result i64)
    (local $packed i64)
    (local.set $packed (call $call_funds))
    (call $ok (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))) (i32.wrap_i64 (local.get $packed))))
"#;

fn funds(items: &[(u8, u128)]) -> Vec<ksana_primitives::Funds> {
    items
        .iter()
        .map(|(asset, amount)| ksana_primitives::Funds {
            asset: id(*asset),
            amount: *amount,
        })
        .collect()
}

/// The arguments of `FUNDS_PROXY`: the callee contract, the raw funds bytes, the callee method.
fn funds_proxy_args(callee: u8, raw_funds: &[u8], method: &str) -> Vec<u8> {
    let mut args = id(callee).to_vec();
    args.extend(u32::try_from(raw_funds.len()).unwrap().to_le_bytes());
    args.extend(raw_funds);
    args.extend(borsh_input(method, b""));
    args
}

#[test]
fn call_funds_reports_the_attached_assets() {
    let vm = Vm::new(&DEVNET).unwrap();
    // top level: the execution layer has already transferred, the VM only records
    let attached = funds(&[(3, 7), (9, 1)]);
    let (_, r) = vm.call(
        host_with(&[(2, FUNDS_READER)]),
        CallRequest {
            contract: id(2),
            caller: id(0xAA),
            method: "x".into(),
            args: vec![],
            funds: attached.clone(),
            allow_init: false,
        },
        GAS,
    );
    assert_eq!(success(r).0, borsh::to_vec(&attached).unwrap());
    // an empty list when nothing is attached
    let (_, r) = call(&vm, host_with(&[(2, FUNDS_READER)]), 2, "x", b"", GAS);
    assert_eq!(success(r).0, 0u32.to_le_bytes());
}

/// Returns the result of `native_tx`: the Borsh encoding of the transaction carrying this op when there is one, empty
/// when there is none (-1).
const NATIVE_READER: &str = r#"
  (func (export "ksana_call") (param i32 i32) (result i64)
    (local $packed i64)
    (local.set $packed (call $native_tx))
    (if (i64.eq (local.get $packed) (i64.const -1))
      (then (return (call $ok (i32.const 0) (i32.const 0)))))
    (call $ok (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))) (i32.wrap_i64 (local.get $packed))))
"#;

#[test]
fn native_tx_returns_the_carrying_transaction_and_charges_per_byte() {
    let vm = Vm::new(&DEVNET).unwrap();
    let tx = |script_len: usize| NativeTxInfo {
        txid: id(7),
        outputs: vec![
            NativeOutput {
                value: 0,
                script: vec![0x6a; 3],
            },
            NativeOutput {
                value: 5_000,
                script: vec![0x51; script_len],
            },
        ],
    };
    let with_tx = |native: Option<NativeTxInfo>| {
        let mut host = host_with(&[(2, NATIVE_READER)]);
        host.native = native;
        host
    };
    // no transaction carrying this op (path B, read-only calls): -1, only the fixed part is charged
    let (_, r) = call(&vm, with_tx(None), 2, "x", b"", GAS);
    let (out, gas_none) = success(r);
    assert!(out.is_empty());
    // path C: returns the transaction ID and all outputs, charging extra gas per returned byte
    let small = tx(34);
    let encoded = borsh::to_vec(&small).unwrap();
    let (_, r) = call(&vm, with_tx(Some(small)), 2, "x", b"", GAS);
    let (out, gas_small) = success(r);
    assert_eq!(out, encoded);
    assert!(gas_small - gas_none >= gas::NATIVE_TX_PER_BYTE * encoded.len() as u64);
    let (_, r) = call(&vm, with_tx(Some(tx(34 + 1_000))), 2, "x", b"", GAS);
    let (_, gas_big) = success(r);
    assert!(gas_big - gas_small >= 1_000, "{gas_big} {gas_small}");
    // gas insufficient for the fixed part: out of gas
    let (_, r) = call(&vm, with_tx(Some(tx(34))), 2, "x", b"", gas_none - 1);
    assert!(
        matches!(r, Ok(CallOutcome::Failure(FailureReason::OutOfGas))),
        "{r:?}"
    );
}

#[test]
fn nested_calls_move_funds_from_the_calling_contract() {
    let vm = Vm::new(&DEVNET).unwrap();
    let with_balance = || {
        let mut host = host_with(&[(1, FUNDS_PROXY), (2, FUNDS_READER), (3, ECHO)]);
        host.balances.insert((id(9), id(1)), 10);
        host
    };
    let attached = funds(&[(9, 4)]);
    let raw = borsh::to_vec(&attached).unwrap();
    let (host, r) = call(
        &vm,
        with_balance(),
        1,
        "go",
        &funds_proxy_args(2, &raw, "x"),
        GAS,
    );
    assert_eq!(success(r).0, raw);
    assert_eq!(host.balances[&(id(9), id(1))], 6);
    assert_eq!(host.balances[&(id(9), id(2))], 4);
    // funds length 0: nothing attached
    let (host, r) = call(
        &vm,
        with_balance(),
        1,
        "go",
        &funds_proxy_args(2, b"", "x"),
        GAS,
    );
    assert_eq!(success(r).0, 0u32.to_le_bytes());
    assert_eq!(host.balances[&(id(9), id(1))], 10);
    // insufficient balance (only the calling contract's own balance can be moved)
    let too_much = borsh::to_vec(&funds(&[(9, 11)])).unwrap();
    let (_, r) = call(
        &vm,
        with_balance(),
        1,
        "go",
        &funds_proxy_args(2, &too_much, "x"),
        GAS,
    );
    failure(r);

    // 10,000 gas per attached asset item: with the same callee and input, attaching 1 item vs none differs by exactly
    // `FUNDS_PER_ASSET`
    let one = borsh::to_vec(&funds(&[(9, 1)])).unwrap();
    let (_, r) = call(
        &vm,
        with_balance(),
        1,
        "go",
        &funds_proxy_args(3, &one, "x"),
        GAS,
    );
    let attached = success(r).1;
    let (_, r) = call(
        &vm,
        with_balance(),
        1,
        "go",
        &funds_proxy_args(3, b"", "x"),
        GAS,
    );
    let plain = success(r).1;
    assert_eq!(attached - plain, ksana_vm::gas::FUNDS_PER_ASSET);
}

#[test]
fn malformed_funds_are_rejected() {
    let vm = Vm::new(&DEVNET).unwrap();
    let host = || {
        let mut host = host_with(&[(1, FUNDS_PROXY), (2, FUNDS_READER)]);
        host.balances.insert((id(9), id(1)), 100);
        host.balances.insert((id(8), id(1)), 100);
        host
    };
    let mut trailing = borsh::to_vec(&funds(&[(9, 1)])).unwrap();
    trailing.push(0);
    let too_many = borsh::to_vec(&funds(&(0..=16u8).map(|i| (i, 1)).collect::<Vec<_>>())).unwrap();
    for raw in [
        vec![1, 0, 0],                                     // truncated
        trailing,                                          // trailing bytes
        borsh::to_vec(&funds(&[(9, 1), (8, 1)])).unwrap(), // out of order
        borsh::to_vec(&funds(&[(9, 1), (9, 1)])).unwrap(), // duplicate
        borsh::to_vec(&funds(&[(9, 0)])).unwrap(),         // amount 0
        too_many, // more than 16 items (the encoding exceeds the limit too)
    ] {
        let (host, r) = call(&vm, host(), 1, "go", &funds_proxy_args(2, &raw, "x"), GAS);
        assert!(
            matches!(failure(r), FailureReason::Rejected(_)),
            "funds {raw:?} should be rejected"
        );
        assert_eq!(host.balances[&(id(9), id(1))], 100);
    }
}

/// A proxy for catchable calls (D-073): args = callee contract ID(32) ‖ gas limit u64 ‖ input; returns the result of
/// `try_call_contract` (Borsh `(status: u8, data: Vec<u8>)`).
const TRY_PROXY: &str = r#"
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32) (local $packed i64)
    (local.set $a (call $args_ptr (local.get $p)))
    (local.set $packed
      (call $try_call_contract
        (local.get $a)
        (i32.add (local.get $a) (i32.const 40))
        (i32.sub (call $args_len (local.get $p)) (i32.const 40))
        (i32.const 0) (i32.const 0)
        (i64.load (i32.add (local.get $a) (i32.const 32)))))
    (call $ok (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))) (i32.wrap_i64 (local.get $packed))))
"#;

fn try_args(callee: u8, gas: u64, method: &str) -> Vec<u8> {
    let mut args = id(callee).to_vec();
    args.extend(gas.to_le_bytes());
    args.extend(borsh_input(method, b""));
    args
}

fn try_status(out: &[u8]) -> (u8, Vec<u8>) {
    borsh::from_slice(out).unwrap()
}

#[test]
fn try_call_gas_limit_is_exact() {
    // part of the callee's consumption is after the last fuel check (as in the D-020 test): the sub-call's limit is
    // exact as well
    let tail: String = (0..40)
        .map(|i| format!("(drop (i32.add (i32.const {i}) (i32.const 1)))\n"))
        .collect();
    let callee = format!(
        r#"(func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
             (local $r i64)
             (local.set $r (call $ok (i32.const 0) (i32.const 0)))
             {tail}
             (local.get $r))"#
    );
    let vm = Vm::new(&DEVNET).unwrap();
    let host = || host_with(&[(1, TRY_PROXY), (2, callee.as_str())]);
    // the callee's own consumption = the top-level call's consumption − the code loading cost (in a sub-call the code
    // loading cost is paid by the caller)
    let (h, r) = call(&vm, host(), 2, "x", b"", GAS);
    let code_len = u64::try_from(h.contracts[&id(2)].code.len()).unwrap();
    let own = success(r).1 - code_len * ksana_vm::gas::CALL_PER_CODE_BYTE;

    let (_, r) = call(&vm, host(), 1, "go", &try_args(2, own, "x"), GAS);
    assert_eq!(
        try_status(&success(r).0).0,
        0,
        "the limit is exactly enough"
    );
    let (_, r) = call(&vm, host(), 1, "go", &try_args(2, own + 1, "x"), GAS);
    assert_eq!(try_status(&success(r).0).0, 0);
    for limit in own - 80..own {
        let (_, r) = call(&vm, host(), 1, "go", &try_args(2, limit, "x"), GAS);
        assert_eq!(try_status(&success(r).0), (2, vec![]), "limit {limit}");
    }
}

#[test]
fn failed_subcalls_use_their_whole_limit_and_reverts_refund() {
    let vm = Vm::new(&DEVNET).unwrap();
    let reverter = r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $err (i32.const 0) (i32.const 2)))"#;
    let trapper = r#"(func (export "ksana_call") (param i32 i32) (result i64) unreachable)"#;
    let host = || host_with(&[(1, TRY_PROXY), (2, reverter), (3, trapper)]);
    let total = |callee: u8, limit: u64| {
        let (_, r) = call(&vm, host(), 1, "go", &try_args(callee, limit, "x"), GAS);
        let (out, used) = success(r);
        (try_status(&out).0, used)
    };
    // revert: the unused gas is refunded, the total consumption is independent of the sub-call limit
    let (s1, a) = total(2, 100_000);
    let (s2, b) = total(2, 200_000);
    assert_eq!((s1, s2), (1, 1));
    assert_eq!(a, b);
    // other failures: the sub-call's whole limit is used up
    let (s1, a) = total(3, 100_000);
    let (s2, b) = total(3, 200_000);
    assert_eq!((s1, s2), (2, 2));
    assert_eq!(b - a, 100_000);
}

#[test]
fn try_call_failures_do_not_leak_state() {
    let vm = Vm::new(&DEVNET).unwrap();
    // the callee traps after writing storage and emitting an event
    let writer = r#"
      (data (i32.const 300) "key")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (call $storage_set (i32.const 300) (i32.const 3) (i32.const 300) (i32.const 3))
        (call $emit_event (i32.const 300) (i32.const 3))
        unreachable)"#;
    let (host, r) = call(
        &vm,
        host_with(&[(1, TRY_PROXY), (2, writer)]),
        1,
        "go",
        &try_args(2, 0, "x"),
        GAS,
    );
    assert_eq!(try_status(&success(r).0), (2, vec![]));
    assert!(host.storage.is_empty());
    assert!(host.events.is_empty());
    assert!(host.checkpoints.is_empty(), "checkpoint removed");
    // callee does not exist, depth exceeded: a failure rather than the whole call failing
    let (_, r) = call(
        &vm,
        host_with(&[(1, TRY_PROXY)]),
        1,
        "go",
        &try_args(9, 0, "x"),
        GAS,
    );
    assert_eq!(try_status(&success(r).0), (2, vec![]));
    // a caller error (method name init) cannot be caught
    let (_, r) = call(
        &vm,
        host_with(&[(1, TRY_PROXY), (2, writer)]),
        1,
        "go",
        &try_args(2, 0, "init"),
        GAS,
    );
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
}

/// A delegate-call proxy: args = target contract ID(32) ‖ input; executes the target contract's code with
/// `delegate_call` and returns its result.
const DELEGATOR: &str = r#"
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32) (local $packed i64)
    (local.set $a (call $args_ptr (local.get $p)))
    (local.set $packed
      (call $delegate_call (local.get $a) (i32.add (local.get $a) (i32.const 32))
        (i32.sub (call $args_len (local.get $p)) (i32.const 32))))
    (call $ok (i32.wrap_i64 (i64.shr_u (local.get $packed) (i64.const 32))) (i32.wrap_i64 (local.get $packed))))
"#;

#[test]
fn delegate_call_keeps_the_current_context() {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut args = id(2).to_vec();
    args.extend(borsh_input("who", b""));
    let (_, r) = call(
        &vm,
        host_with(&[(1, DELEGATOR), (2, WHO)]),
        1,
        "go",
        &args,
        GAS,
    );
    let out = success(r).0;
    assert_eq!(
        &out[..32],
        &id(0xAA),
        "caller is still the top-level caller"
    );
    assert_eq!(&out[32..], &id(1), "self_id is still the delegator");
    // the delegated code writes the delegator's storage
    let (host, r) = call(
        &vm,
        host_with(&[(1, DELEGATOR), (2, STORE)]),
        1,
        "go",
        &{
            let mut a = id(2).to_vec();
            a.extend(borsh_input("x", b"key"));
            a
        },
        GAS,
    );
    success(r);
    assert!(host.storage.keys().all(|(contract, _)| *contract == id(1)));
    assert!(!host.storage.is_empty());
    // target does not exist: the whole call fails
    let mut args = id(9).to_vec();
    args.extend(borsh_input("who", b""));
    let (_, r) = call(&vm, host_with(&[(1, DELEGATOR)]), 1, "go", &args, GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
}

#[test]
fn environment_host_functions() {
    // returns block_timestamp ‖ chain_id ‖ gas_left (8 little-endian bytes each)
    let env = r#"
      (func (export "ksana_call") (param i32 i32) (result i64)
        (i64.store (i32.const 300) (call $block_timestamp))
        (i64.store (i32.const 308) (call $chain_id))
        (i64.store (i32.const 316) (call $gas_left))
        (call $ok (i32.const 300) (i32.const 24)))"#;
    let vm = Vm::new(&DEVNET).unwrap();
    let mut host = host_with(&[(1, env)]);
    host.timestamp = 1_700_000_042;
    let (_, r) = call(&vm, host, 1, "x", b"", GAS);
    let (out, used) = success(r);
    let word = |i: usize| u64::from_le_bytes(out[i * 8..i * 8 + 8].try_into().unwrap());
    assert_eq!(word(0), 1_700_000_042);
    assert_eq!(word(1), u64::from(DEVNET.chain_id));
    // gas_left is read before this call's total consumption: between "limit − total consumption" and the limit
    assert!(word(2) < GAS && word(2) > GAS - used, "{} {used}", word(2));
}

#[test]
fn asset_burn_reduces_balance_and_supply() {
    let burner = r#"
      (data (i32.const 300) "\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09\09")
      (data (i32.const 340) "\1e")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (call $asset_burn (i32.const 300) (i32.const 340))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    let vm = Vm::new(&DEVNET).unwrap();
    let host = |balance: u128| {
        let mut host = host_with(&[(1, burner)]);
        host.assets.insert(
            id(9),
            (
                ksana_vm::AssetMetaInput {
                    name: "T".into(),
                    symbol: "T".into(),
                    decimals: 0,
                },
                100,
                id(1),
            ),
        );
        host.balances.insert((id(9), id(1)), balance);
        host
    };
    let (h, r) = call(&vm, host(100), 1, "x", b"", GAS);
    success(r);
    assert_eq!(h.balances[&(id(9), id(1))], 70);
    assert_eq!(h.assets[&id(9)].1, 70);
    // only this contract's balance can be burned
    let (_, r) = call(&vm, host(29), 1, "x", b"", GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
}

/// A factory: args = code_hash(32) ‖ salt(32) ‖ init args; creates a contract with `create_contract` and returns
/// its ID.
const FACTORY: &str = r#"
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32)
    (local.set $a (call $args_ptr (local.get $p)))
    (call $create_contract
      (local.get $a)
      (i32.add (local.get $a) (i32.const 32))
      (i32.add (local.get $a) (i32.const 64))
      (i32.sub (call $args_len (local.get $p)) (i32.const 64))
      (i32.const 0) (i32.const 0)
      (i32.const 500))
    (call $ok (i32.const 500) (i32.const 32)))
"#;

#[test]
fn create_contract_derives_the_address_and_runs_init() {
    let vm = Vm::new(&DEVNET).unwrap();
    // the created contract: init records the caller
    let child = r#"
      (data (i32.const 300) "creator")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (call $caller (i32.const 400))
        (call $storage_set (i32.const 300) (i32.const 7) (i32.const 400) (i32.const 32))
        (call $ok (i32.const 0) (i32.const 0)))"#;
    let child_code = common::module(child);
    let code_hash = sha256(&child_code);
    let mut host = host_with(&[(1, FACTORY)]);
    host.codes.insert(code_hash, child_code);
    let args = |hash: [u8; 32], salt: u8| {
        let mut a = hash.to_vec();
        a.extend([salt; 32]);
        a
    };
    let (host, r) = call(&vm, host, 1, "go", &args(code_hash, 7), GAS);
    let out = success(r).0;
    let mut preimage = id(1).to_vec();
    preimage.extend([7u8; 32]);
    preimage.extend(code_hash);
    let expected = sha256(&preimage);
    assert_eq!(out, expected);
    assert_eq!(host.creators[&expected], id(1));
    assert_eq!(
        host.storage[&(expected, b"creator".to_vec())],
        id(1).to_vec(),
        "init has the creator as the caller"
    );
    // creating at the same address again, code does not exist: the whole call fails
    let (host, r) = call(&vm, host, 1, "go", &args(code_hash, 7), GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
    let (_, r) = call(&vm, host, 1, "go", &args([3; 32], 8), GAS);
    assert!(matches!(failure(r), FailureReason::Rejected(_)));
}
