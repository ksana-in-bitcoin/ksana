//! The ways a node may run the VM (D-151): the compiled code kept on disk and the pool of instance memory change
//! nothing a call computes, at the limits included, and the pool never runs out.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use common::{id, module, raw, MockHost, IMPORTS, RUNTIME};
use ksana_primitives::DEVNET;
use ksana_vm::{CallOutcome, CallRequest, CodeCache, FailureReason, Vm, VmError, VmOptions};

const GAS: u64 = 100_000_000;

fn pooled(calls: u32) -> Vm {
    let options = VmOptions {
        code_cache: None,
        pooled_calls: calls,
    };
    Vm::with_options(&DEVNET, &options).unwrap()
}

fn cached(dir: &Path) -> Vm {
    let options = VmOptions {
        code_cache: Some(CodeCache {
            dir: dir.to_path_buf(),
            max_bytes: 1 << 30,
        }),
        pooled_calls: 0,
    };
    Vm::with_options(&DEVNET, &options).unwrap()
}

/// A module with the runtime's helpers over a memory of `pages` pages at the start.
fn with_memory(pages: u32, body: &str) -> Vec<u8> {
    let memory = "(memory (export \"memory\") 2)";
    assert!(RUNTIME.contains(memory));
    let runtime = RUNTIME.replace(memory, &format!("(memory (export \"memory\") {pages})"));
    raw(&format!("(module {IMPORTS} {runtime} {body})"))
}

/// Calls contract 2 `times` times, one after the other.
fn looper(times: u32) -> Vec<u8> {
    module(&format!(
        r#"
      (data (i32.const 500) "\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02\02")
      (data (i32.const 540) "\03\00\00\00who\00\00\00\00")
      (func (export "ksana_call") (param i32 i32) (result i64)
        (local $i i32)
        (loop $l
          (drop (call $call_contract (i32.const 500) (i32.const 540) (i32.const 11) (i32.const 0) (i32.const 0)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br_if $l (i32.lt_u (local.get $i) (i32.const {times}))))
        (call $ok (i32.const 0) (i32.const 0)))"#
    ))
}

/// Traps unless the memory past the runtime's own data is zero where it looks, then writes there. `pages` is the
/// size it expects.
fn scribbler(pages: u32, declared: u32) -> Vec<u8> {
    let last = pages * 65536 - 8;
    let middle = pages * 32768;
    with_memory(
        declared,
        &format!(
            r#"
      (func $visit (param $at i32)
        (if (i64.ne (i64.load (local.get $at)) (i64.const 0)) (then unreachable))
        (i64.store (local.get $at) (i64.const -1)))
      (func (export "ksana_call") (param i32 i32) (result i64)
        (if (i32.lt_u (memory.size) (i32.const {pages}))
          (then (drop (memory.grow (i32.sub (i32.const {pages}) (memory.size))))))
        (if (i32.ne (memory.size) (i32.const {pages})) (then unreachable))
        (call $visit (i32.const 1024))
        (call $visit (i32.const {middle}))
        (call $visit (i32.const {last}))
        (if (i32.ne (memory.grow (i32.const 1)) (i32.const -1)) (then unreachable))
        (call $storage_set (i32.const 2048) (i32.const 1) (i32.const 1024) (i32.const 8))
        (call $ok (i32.const 1024) (i32.const 8)))"#
        ),
    )
}

/// A table of the largest size a code may declare, called through its last element, and as many globals as fit a
/// code of about 50 KiB: the most an instance's own data can be asked to hold.
fn wide() -> Vec<u8> {
    let globals: String = (0..9_000)
        .map(|i| format!("(global $g{i} (mut i64) (i64.const {i}))"))
        .collect();
    module(&format!(
        r#"
      {globals}
      (type $f (func (result i32)))
      (table 4096 4096 funcref)
      (elem (i32.const 4095) $seven)
      (func $seven (result i32) (i32.const 7))
      (func (export "ksana_call") (param i32 i32) (result i64)
        (global.set $g8999 (i64.add (global.get $g8998) (i64.const 1)))
        (i64.store (i32.const 1024)
          (i64.add (global.get $g8999) (i64.extend_i32_u (call_indirect (type $f) (i32.const 4095)))))
        (call $ok (i32.const 1024) (i32.const 8)))"#
    ))
}

const WHO: &str = r#"
  (func (export "ksana_call") (param i32 i32) (result i64)
    (call $caller (i32.const 400))
    (call $self_id (i32.const 432))
    (call $ok (i32.const 400) (i32.const 64)))
"#;

type Storage = BTreeMap<([u8; 32], Vec<u8>), Vec<u8>>;

fn run(vm: &Vm, contracts: &[(u8, Vec<u8>)]) -> (Result<CallOutcome, VmError>, Storage) {
    let mut host = MockHost::default();
    for (i, code) in contracts {
        host.deploy(id(*i), code.clone());
    }
    let (host, result) = vm.call(
        host,
        CallRequest {
            contract: id(1),
            caller: id(0xAA),
            method: "who".into(),
            args: Vec::new(),
            funds: Vec::new(),
            allow_init: false,
        },
        GAS,
    );
    (result, host.storage)
}

fn cases() -> Vec<(&'static str, Vec<(u8, Vec<u8>)>, bool)> {
    vec![
        ("echo of the caller", vec![(1, module(WHO))], true),
        (
            "the largest memory, declared",
            vec![(1, scribbler(256, 256))],
            true,
        ),
        (
            "the largest memory, grown to",
            vec![(1, scribbler(256, 2))],
            true,
        ),
        (
            "a memory grown past the limit",
            vec![(1, scribbler(257, 2))],
            false,
        ),
        (
            "15 instances of the largest memory in one call",
            vec![(1, looper(15)), (2, scribbler(256, 256))],
            true,
        ),
        (
            "16 instances of the largest memory in one call: past the total of a call",
            vec![(1, looper(16)), (2, scribbler(256, 256))],
            false,
        ),
        (
            "63 instances after the first",
            vec![(1, looper(63)), (2, module(WHO))],
            true,
        ),
        (
            "64 instances after the first: past the count of a call",
            vec![(1, looper(64)), (2, module(WHO))],
            false,
        ),
        (
            "the largest table and 9 000 globals",
            vec![(1, wide())],
            true,
        ),
        (
            "63 instances of the largest table and 9 000 globals",
            vec![(1, looper(63)), (2, wide())],
            true,
        ),
    ]
}

/// Every case gives the same outcome, gas and writes on memory from the operating system, on a pool, on a pool used
/// before, and with the compiled code read back from disk.
#[test]
fn the_options_change_no_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let plain = Vm::new(&DEVNET).unwrap();
    let pool = pooled(1);
    let first = cached(dir.path());
    let mut expected = Vec::new();
    for (name, contracts, succeeds) in cases() {
        let reference = run(&plain, &contracts);
        match &reference.0 {
            Ok(CallOutcome::Success { .. }) => assert!(succeeds, "{name}: {:?}", reference.0),
            Ok(CallOutcome::Failure(reason)) => {
                assert!(!succeeds, "{name}: {reason:?}");
                assert!(!matches!(reason, FailureReason::OutOfGas), "{name}");
            }
            Err(e) => panic!("{name}: {e:?}"),
        }
        // twice on the pool: the second run gets the memory the first one wrote to
        for round in 0..2 {
            assert_eq!(
                run(&pool, &contracts),
                reference,
                "{name}, pool, round {round}"
            );
        }
        assert_eq!(run(&first, &contracts), reference, "{name}, cache");
        expected.push(reference);
    }
    let (hits, misses) = first.code_cache_use().unwrap();
    assert!(misses > 0, "the first VM compiled the codes");
    drop(first);

    // a second VM over the same directory compiles nothing
    let second = cached(dir.path());
    for ((name, contracts, _), reference) in cases().iter().zip(&expected) {
        assert_eq!(
            &run(&second, contracts),
            reference,
            "{name}, cache read back"
        );
    }
    let (read, compiled) = second.code_cache_use().unwrap();
    assert_eq!(
        compiled, 0,
        "the second VM compiled a code the first one kept"
    );
    assert!(read > hits);
    assert_eq!(plain.code_cache_use(), None);
}

/// A file of the cache that is not what the VM wrote is compiled again: a truncated or overwritten file costs time,
/// not the result. (A file crafted to load is trusted, as the node's binary is.)
#[test]
fn a_damaged_cache_file_is_compiled_again() {
    fn files(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let contracts = vec![(1, looper(3)), (2, module(WHO))];
    let reference = run(&Vm::new(&DEVNET).unwrap(), &contracts);
    assert!(matches!(reference.0, Ok(CallOutcome::Success { .. })));
    assert_eq!(run(&cached(dir.path()), &contracts), reference);

    let mut found = Vec::new();
    files(dir.path(), &mut found);
    assert!(found.len() >= 2, "one file per code: {found:?}");
    for (i, path) in found.iter().enumerate() {
        let bytes = std::fs::read(path).unwrap();
        let damaged = if i % 2 == 0 {
            bytes[..bytes.len() / 2].to_vec()
        } else {
            vec![0xA5; bytes.len()]
        };
        std::fs::write(path, damaged).unwrap();
    }
    let vm = cached(dir.path());
    assert_eq!(run(&vm, &contracts), reference);
    assert_eq!(
        vm.code_cache_use().unwrap().0,
        0,
        "nothing damaged was used"
    );
}

/// More calls at once than the pool has room for wait for one another: each of them may create the full count of
/// instances, and none is refused.
#[test]
fn calls_beyond_the_pool_wait_for_it() {
    let vm = Arc::new(pooled(1));
    let contracts = Arc::new(vec![(1, looper(63)), (2, module(WHO))]);
    let reference = run(&Vm::new(&DEVNET).unwrap(), &contracts);
    assert!(matches!(reference.0, Ok(CallOutcome::Success { .. })));
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let (vm, contracts) = (Arc::clone(&vm), Arc::clone(&contracts));
            std::thread::spawn(move || (0..5).map(|_| run(&vm, &contracts)).collect::<Vec<_>>())
        })
        .collect();
    for thread in threads {
        for result in thread.join().unwrap() {
            assert_eq!(result, reference);
        }
    }
}

/// A call that panics in the host gives its place in the pool back.
#[test]
fn a_call_that_ends_early_returns_its_place() {
    let vm = pooled(1);
    let mut host = MockHost::default();
    host.deploy(
        id(1),
        module(
            r#"(func (export "ksana_call") (param i32 i32) (result i64)
                 (drop (call $storage_get (i32.const 0) (i32.const 1))) (call $ok (i32.const 0) (i32.const 0)))"#,
        ),
    );
    host.fail_fatal = true;
    let request = || CallRequest {
        contract: id(1),
        caller: id(0xAA),
        method: "x".into(),
        args: Vec::new(),
        funds: Vec::new(),
        allow_init: false,
    };
    let (mut host, result) = vm.call(host, request(), GAS);
    assert!(result.is_err(), "{result:?}");
    host.fail_fatal = false;
    let (_, result) = vm.call(host, request(), GAS);
    assert!(
        matches!(result, Ok(CallOutcome::Success { .. })),
        "{result:?}"
    );
}
