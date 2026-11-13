//! Gas table calibration (SPEC §13.1, §14): how long the machine it runs on takes per unit of gas, by what the gas is
//! spent on, and so how long a block that spends `BLOCK_GAS_LIMIT` on it alone takes to execute.
//!
//! It measures and prints; it asserts nothing about the times. Run it on an idle machine with
//! `cargo test --release -p ksana-vm --test gas_calibration -- --ignored --nocapture`.
//!
//! The host is the in-memory one of the VM's tests, so the state's cost (the Merkle tree and the database) is not in
//! these figures: they are the VM's side of each host function.

// a measurement reads the clock; nothing here is consensus code
#![allow(clippy::disallowed_methods)]

mod common;

use std::time::{Duration, Instant};

use common::{id, module, raw, MockHost};
use ksana_primitives::params::DEVNET;
use ksana_vm::{CallOutcome, CallRequest, FailureReason, Vm, VmError};

/// A block's gas (`BLOCK_GAS_LIMIT`).
const BLOCK: u64 = 100_000_000;

fn request(contract: u8, args: &[u8]) -> CallRequest {
    CallRequest {
        contract: id(contract),
        caller: id(0xAA),
        method: "x".into(),
        args: args.to_vec(),
        funds: Vec::new(),
        allow_init: false,
    }
}

fn report(name: &str, gas: u64, elapsed: Duration, note: &str) {
    let ns_per_gas = elapsed.as_nanos() as f64 / gas as f64;
    println!(
        "{name:<44} {gas:>12} gas {:>9.1} ms {ns_per_gas:>8.2} ns/gas {:>8.2} s/block  {note}",
        elapsed.as_secs_f64() * 1e3,
        ns_per_gas * BLOCK as f64 / 1e9,
    );
}

/// A contract whose `ksana_call` repeats `body` until the gas runs out.
fn looping(body: &str, extra: &str) -> Vec<u8> {
    module(&format!(
        r#"{extra}
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $i i64)
    (loop $again
      {body}
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br $again))
    (i64.const 0))"#
    ))
}

/// Runs the looping contract once with a block's gas, after a short run that compiles it.
fn burn(name: &str, body: &str, extra: &str, args: &[u8], prepare: impl Fn(&mut MockHost)) {
    let vm = Vm::new(&DEVNET).unwrap();
    let mut host = MockHost::default();
    host.deploy(id(1), looping(body, extra));
    prepare(&mut host);
    let (host, warm) = vm.call(host, request(1, args), 1_000_000);
    match warm {
        Ok(CallOutcome::Failure(FailureReason::OutOfGas)) => {}
        other => {
            println!("{name:<44} did not run out of gas: {other:?}");
            return;
        }
    }
    let start = Instant::now();
    let (_, result) = vm.call(host, request(1, args), BLOCK);
    let elapsed = start.elapsed();
    match result {
        Ok(CallOutcome::Failure(FailureReason::OutOfGas)) => report(name, BLOCK, elapsed, ""),
        other => println!("{name:<44} did not run out of gas: {other:?}"),
    }
}

fn gas_of(result: Result<CallOutcome, VmError>, limit: u64) -> u64 {
    match result {
        Ok(CallOutcome::Success { gas_used, .. }) => gas_used,
        Ok(CallOutcome::Failure(FailureReason::OutOfGas)) => limit,
        other => panic!("unexpected outcome: {other:?}"),
    }
}

const NO_PREPARE: fn(&mut MockHost) = |_| {};

/// BIP340 test vector 0: public key, message, signature.
const SCHNORR: &str = "F9308A019258C31049344F85F89D5229B531C845836F99B08601F113BCE036F9\
0000000000000000000000000000000000000000000000000000000000000000\
E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA8215\
25F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0";

#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn instructions() {
    burn("i64.add loop", "", "", &[], NO_PREPARE);
    burn(
        "i64.div_u",
        "(local.set $i (i64.add (local.get $i) (i64.div_u (i64.const -1) (i64.or (local.get $i) (i64.const 3)))))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "i64.rem_s",
        "(local.set $i (i64.add (local.get $i) (i64.rem_s (i64.const 0x7fffffffffffffff) (i64.or (local.get $i) (i64.const 3)))))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "i64.popcnt, clz, ctz, rotl",
        "(local.set $i (i64.add (local.get $i) (i64.rotl (i64.add (i64.popcnt (local.get $i)) (i64.add (i64.clz (local.get $i)) (i64.ctz (local.get $i)))) (i64.const 7))))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "call of an empty function",
        "(call $nothing)",
        "(func $nothing)",
        &[],
        NO_PREPARE,
    );
    burn(
        "call_indirect of an empty function",
        "(call_indirect (type $t) (i32.const 0))",
        "(type $t (func)) (table 1 1 funcref) (elem (i32.const 0) $nothing) (func $nothing)",
        &[],
        NO_PREPARE,
    );
    burn(
        "i64.load and i64.store",
        "(i64.store (i32.const 0) (i64.add (i64.load (i32.const 0)) (local.get $i)))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "i64.load at scattered addresses",
        "(local.set $i (i64.add (local.get $i) (i64.load (i32.and (i32.mul (i32.wrap_i64 (local.get $i)) (i32.const 40503)) (i32.const 0x1fff8)))))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "memory.fill of 60 000 bytes",
        "(memory.fill (i32.const 0) (i32.const 7) (i32.const 60000))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "memory.copy of 30 000 bytes",
        "(memory.copy (i32.const 30000) (i32.const 0) (i32.const 30000))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "memory.fill of 1 byte",
        "(memory.fill (i32.const 0) (i32.const 7) (i32.const 1))",
        "",
        &[],
        NO_PREPARE,
    );
}

#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn host_functions() {
    burn(
        "block_height (100)",
        "(drop (call $block_height))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "caller (100)",
        "(call $caller (i32.const 0))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "gas_left (100)",
        "(drop (call $gas_left))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "sha256 of 0 bytes (500)",
        "(call $sha256 (i32.const 0) (i32.const 0) (i32.const 100))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "sha256 of 60 000 bytes (500 + 1/B)",
        "(call $sha256 (i32.const 0) (i32.const 60000) (i32.const 60000))",
        "",
        &[],
        NO_PREPARE,
    );
    let vector = hex::decode(SCHNORR).unwrap();
    burn(
        "verify_schnorr, valid (50 000)",
        "(if (i32.eqz (call $verify_schnorr (call $args_ptr (local.get $p)) (i32.add (call $args_ptr (local.get $p)) (i32.const 32)) (i32.add (call $args_ptr (local.get $p)) (i32.const 64)))) (then unreachable))",
        "",
        &vector,
        NO_PREPARE,
    );
    burn(
        "emit_event of 0 bytes (1 000)",
        "(call $emit_event (i32.const 0) (i32.const 0))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "emit_event of 1 024 bytes (1 000 + 5/B)",
        "(call $emit_event (i32.const 0) (i32.const 1024))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "storage_set, 32 + 32 bytes, new keys",
        "(i64.store (i32.const 0) (local.get $i)) (call $storage_set (i32.const 0) (i32.const 32) (i32.const 64) (i32.const 32))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "storage_set, 32 + 32 bytes, one key",
        "(call $storage_set (i32.const 0) (i32.const 32) (i32.const 64) (i32.const 32))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "storage_get, missing key (2 000 + 1/B)",
        "(drop (call $storage_get (i32.const 0) (i32.const 32)))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "storage_remove, missing key (5 000 + 1/B)",
        "(call $storage_remove (i32.const 0) (i32.const 32))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "asset_balance (2 000)",
        "(call $asset_balance (i32.const 0) (i32.const 32) (i32.const 64))",
        "",
        &[],
        NO_PREPARE,
    );
    burn(
        "call_funds, none (100 + 1/B)",
        "(drop (call $call_funds))",
        "",
        &[],
        NO_PREPARE,
    );
}

/// A contract with a memory of `pages` pages at once that writes one byte to each 4 KiB of it: every write is a page
/// the kernel has to hand out, at the price of a store.
fn toucher(pages: u32) -> Vec<u8> {
    raw(&format!(
        r#"(module
  (memory (export "memory") {pages})
  (func (export "ksana_alloc") (param i32) (result i32) (i32.const 65536))
  (func (export "ksana_call") (param i32 i32) (result i64)
    (local $a i32)
    (local.set $a (i32.const 131072))
    (loop $next
      (i32.store8 (local.get $a) (i32.const 1))
      (local.set $a (i32.add (local.get $a) (i32.const 4096)))
      (br_if $next (i32.lt_u (local.get $a) (i32.const {end}))))
    (i32.store8 (i32.const 0) (i32.const 1))
    (i32.store (i32.const 1) (i32.const 0))
    (i64.const 5)))"#,
        end = pages * 65536
    ))
}

/// Top-level calls of one contract, one after the other, until they have used a block's gas.
fn calls(name: &str, vm: &Vm, mut host: MockHost, contract: u8, note: &str) {
    let (mut gas, mut count) = (0u64, 0u32);
    let start = Instant::now();
    while gas < BLOCK && start.elapsed() < Duration::from_secs(60) {
        let (back, result) = vm.call(host, request(contract, &[]), 10_000_000);
        host = back;
        gas += gas_of(result, 10_000_000);
        count += 1;
    }
    report(name, gas, start.elapsed(), &format!("{count} calls {note}"));
}

#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn instances_and_memory() {
    let vm = Vm::new(&DEVNET).unwrap();
    for (name, contract, code) in [
        (
            "top-level call, trivial contract",
            1u8,
            module(
                r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $ok (i32.const 0) (i32.const 0)))"#,
            ),
        ),
        ("top-level call touching 16 MiB by page", 2, toucher(256)),
        ("top-level call touching 1 MiB by page", 3, toucher(16)),
    ] {
        let mut host = MockHost::default();
        host.deploy(id(contract), code);
        let (host, warm) = vm.call(host, request(contract, &[]), 10_000_000);
        let each = gas_of(warm, 10_000_000);
        calls(name, &vm, host, contract, &format!("of {each} gas"));
    }
}

/// A module of about `bytes` bytes that differs with `seed`: `funcs` functions of straight-line arithmetic.
fn bulky(seed: u64, funcs: usize, bytes: usize) -> Vec<u8> {
    let per_func = bytes / funcs / 24;
    let mut body = String::new();
    for f in 0..funcs {
        body.push_str(&format!("(func $f{f} (param $x i64) (result i64)\n"));
        for s in 0..per_func {
            let c = seed
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add((f * per_func + s) as u64)
                | 0x4000_0000_0000_0000;
            body.push_str(&format!(
                "(local.set $x (i64.mul (i64.add (local.get $x) (i64.const {c})) (i64.const {})))\n",
                c | 1
            ));
        }
        body.push_str("(local.get $x))\n");
    }
    body.push_str(
        r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $ok (i32.const 0) (i32.const 0)))"#,
    );
    module(&body)
}

/// A module of about `bytes` bytes that differs with `seed`: one function of nested branches on a `br_table`.
fn branchy(seed: u64, bytes: usize) -> Vec<u8> {
    let arms = bytes / 40;
    let mut body = String::from("(func $f (param $x i64) (result i64)\n");
    for a in 0..arms {
        let c = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(a as u64)
            | 0x4000_0000_0000_0000;
        body.push_str(&format!(
            "(if (i64.lt_u (local.get $x) (i64.const {c})) (then (local.set $x (i64.xor (local.get $x) (i64.const {c})))) (else (local.set $x (i64.add (local.get $x) (i64.const {c})))))\n"
        ));
    }
    body.push_str("(local.get $x))\n");
    body.push_str(
        r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $ok (i32.const 0) (i32.const 0)))"#,
    );
    module(&body)
}

/// A module of about `bytes` bytes that differs with `seed`: one function of branches over `locals` locals, each of
/// them written in a branch and all of them read at the end.
fn crowded(seed: u64, locals: usize, bytes: usize) -> Vec<u8> {
    let arms = bytes.saturating_sub(locals * 4) / 28;
    let mut body = String::from("(func $f (param $x i64) (result i64)\n");
    body.push_str(&"(local i64)".repeat(locals));
    for a in 0..arms {
        let c = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(a as u64)
            | 0x4000_0000_0000_0000;
        let l = 1 + a % locals;
        body.push_str(&format!(
            "(if (i64.lt_u (local.get $x) (i64.const {c})) (then (local.set {l} (i64.add (local.get {l}) (local.get $x)))))\n"
        ));
    }
    for l in 1..=locals {
        body.push_str(&format!(
            "(local.set $x (i64.xor (local.get $x) (local.get {l})))\n"
        ));
    }
    body.push_str("(local.get $x))\n");
    body.push_str(
        r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $ok (i32.const 0) (i32.const 0)))"#,
    );
    module(&body)
}

/// A module of about `bytes` bytes that differs with `seed`: one function of branches nested `depth` deep.
fn nested(seed: u64, depth: usize, bytes: usize) -> Vec<u8> {
    let groups = bytes / 24 / depth;
    let mut body = String::from("(func $f (param $x i64) (result i64)\n");
    for g in 0..groups {
        for d in 0..depth {
            let c = seed
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add((g * depth + d) as u64)
                | 0x4000_0000_0000_0000;
            body.push_str(&format!(
                "(if (i64.lt_u (local.get $x) (i64.const {c})) (then (local.set $x (i64.add (local.get $x) (i64.const {c})))\n"
            ));
        }
        body.push_str(&"))".repeat(depth));
        body.push('\n');
    }
    body.push_str("(local.get $x))\n");
    body.push_str(
        r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $ok (i32.const 0) (i32.const 0)))"#,
    );
    module(&body)
}

/// First calls of contracts the node has not compiled: what a call costs in time when its code is not in the
/// compilation cache (after a restart, or past the cache's 256 entries), against the gas it is charged.
#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn compilation() {
    const COUNT: u64 = 12;
    type Shape = Box<dyn Fn(u64) -> Vec<u8>>;
    let shapes: Vec<(&str, Shape)> = vec![
        ("60 KiB, 1 function", Box::new(|s| bulky(s, 1, 60_000))),
        ("60 KiB, 16 functions", Box::new(|s| bulky(s, 16, 60_000))),
        ("60 KiB, 400 functions", Box::new(|s| bulky(s, 400, 60_000))),
        (
            "60 KiB, 1 function of branches",
            Box::new(|s| branchy(s, 60_000)),
        ),
        ("8 KiB, 1 function", Box::new(|s| bulky(s, 1, 8_000))),
        (
            "60 KiB, branches over 1 000 locals",
            Box::new(|s| crowded(s, 1_000, 60_000)),
        ),
        (
            "60 KiB, branches over 8 000 locals",
            Box::new(|s| crowded(s, 8_000, 60_000)),
        ),
        (
            "60 KiB, branches nested 100 deep",
            Box::new(|s| nested(s, 100, 60_000)),
        ),
        (
            "60 KiB, branches nested 1 000 deep",
            Box::new(|s| nested(s, 1_000, 60_000)),
        ),
    ];
    for parallel in [true, false] {
        for (shape, make) in &shapes {
            let vm = Vm::with_parallel_compilation(&DEVNET, parallel).unwrap();
            let mut host = MockHost::default();
            let mut size = 0;
            for seed in 0..COUNT {
                let code = make(seed);
                size = code.len();
                host.deploy([seed as u8 + 1; 32], code);
            }
            for (pass, label) in [(0, "first call"), (1, "again")] {
                let mut gas = 0;
                let start = Instant::now();
                for seed in 0..COUNT {
                    let (back, result) = vm.call(host, request(seed as u8 + 1, &[]), 10_000_000);
                    host = back;
                    gas += gas_of(result, 10_000_000);
                }
                let elapsed = start.elapsed();
                let _ = pass;
                report(
                    &format!("{label}: {shape}"),
                    gas,
                    elapsed,
                    &format!(
                        "{size} B, {:.1} ms a call, parallel={parallel}",
                        elapsed.as_secs_f64() * 1e3 / COUNT as f64
                    ),
                );
            }
        }
    }
}
