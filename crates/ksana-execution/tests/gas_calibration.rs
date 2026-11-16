//! Gas table calibration at the level of a block (SPEC §13.1, §14): how long this machine takes to execute blocks
//! built to be slow for their gas, on the node's own state store (RocksDB and the Merkle tree), and to commit them.
//!
//! It measures and prints; it asserts nothing about the times. Run it on an idle machine with
//! `cargo test --release -p ksana-execution --test gas_calibration -- --ignored --nocapture --test-threads=1`.
//!
//! The VM runs as a node's does by default (D-151): compiled code kept on disk and one call on pooled memory. With
//! `KSANA_GAS_CALIBRATION_PLAIN=1` it runs with neither, as every node did before D-151. The tree a block's check
//! computed is committed either way.
//!
//! A block's entries are kept within `MAX_BLOCK_DATA_BYTES` by their Borsh length, which is at least their length in
//! a payload.

// a measurement reads the clock; nothing here is consensus code
#![allow(clippy::disallowed_methods)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::probe::call;
use common::*;
use ksana_execution::{builtin, BlockEnv, Executor, GenesisConfig, OpStatus, VmOptions};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, BtcAnchor, Entry, DEVNET};
use ksana_state::StateDb;

const FEE: u128 = 1_000;

struct Bench {
    exec: Executor,
    db: StateDb,
    _dir: tempfile::TempDir,
    height: u64,
    user: User,
    nonce: u64,
}

struct Timing {
    gas: u64,
    failed: usize,
    execute: Duration,
    root: Duration,
    commit: Duration,
}

impl Bench {
    fn new() -> Self {
        let user = user("gas calibration");
        let dir = tempfile::tempdir().unwrap();
        let mut db = StateDb::open(&dir.path().join("state")).unwrap();
        let genesis = GenesisConfig::devnet(vec![(user.account, u128::MAX / 4)]);
        db.commit(0, &genesis.writes().unwrap()).unwrap();
        Bench {
            exec: if std::env::var_os("KSANA_GAS_CALIBRATION_PLAIN").is_some() {
                Executor::new(NO_REWARD).unwrap()
            } else {
                let options = VmOptions::in_data_dir(
                    dir.path(),
                    VmOptions::DEFAULT_CODE_CACHE_MIB,
                    VmOptions::DEFAULT_POOLED_CALLS,
                );
                Executor::with_options(NO_REWARD, &options).unwrap()
            },
            db,
            _dir: dir,
            height: 0,
            user,
            nonce: 0,
        }
    }

    /// An entry of the bench's user with the next nonce.
    fn entry(&mut self, max_gas: u64, action: Action) -> Entry {
        self.nonce += 1;
        sponsored(&self.user, self.nonce - 1, FEE, max_gas, action)
    }

    /// Entries made by `action` for as long as a block's data has room for them, at most `most`.
    fn fill(&mut self, most: usize, max_gas: u64, action: impl Fn(usize) -> Action) -> Vec<Entry> {
        let (mut entries, mut bytes) = (Vec::new(), 0);
        while entries.len() < most {
            let entry = sponsored(&self.user, self.nonce, FEE, max_gas, action(entries.len()));
            bytes += borsh::to_vec(&entry).unwrap().len();
            if bytes > DEVNET.max_block_data_bytes {
                break;
            }
            self.nonce += 1;
            entries.push(entry);
        }
        entries
    }

    fn block(&mut self, entries: &[Entry]) -> Timing {
        let env = BlockEnv {
            height: self.height + 1,
            btc_anchor: BtcAnchor {
                height: 200,
                hash: [9; 32],
            },
            producer: user("producer").account,
            timestamp: 1_700_000_000 + 10 * (self.height + 1),
            rules: self.exec.params().rules(self.height + 1),
            unlockable: Vec::new(),
        };
        let view = Arc::new(self.db.view().unwrap());
        let start = Instant::now();
        let out = self
            .exec
            .execute_block(view, Arc::new(TestBtc::default()), &env, entries)
            .unwrap();
        let execute = start.elapsed();
        // a node that accepts a block computes the root to compare it with the header's, then commits
        let start = Instant::now();
        self.db.preview_root(&out.writes).unwrap();
        let root = start.elapsed();
        let start = Instant::now();
        self.db.commit(self.height + 1, &out.writes).unwrap();
        let commit = start.elapsed();
        self.height += 1;
        let failures: Vec<&OpStatus> = out
            .receipts
            .iter()
            .map(|r| &r.status)
            .filter(|s| **s != OpStatus::Success)
            .collect();
        if let Some(status) = failures.first() {
            println!("  a failed op: {status:?}");
        }
        let failed = failures.len();
        Timing {
            gas: out.receipts.iter().map(|r| r.gas_used).sum(),
            failed,
            execute,
            root,
            commit,
        }
    }

    fn measure(&mut self, name: &str, entries: &[Entry]) -> Timing {
        let t = self.block(entries);
        let total = (t.execute + t.root + t.commit).as_secs_f64();
        println!(
            "{name:<52} {:>4} ops {:>4} failed {:>11} gas  execute {:>9.1} ms  root {:>7.1} ms  commit {:>7.1} ms  {:>8.2} ns/gas  {:>7.2} s per 100M gas",
            entries.len(),
            t.failed,
            t.gas,
            t.execute.as_secs_f64() * 1e3,
            t.root.as_secs_f64() * 1e3,
            t.commit.as_secs_f64() * 1e3,
            total * 1e9 / t.gas.max(1) as f64,
            total * 1e8 / t.gas.max(1) as f64,
        );
        t
    }

    /// Deploys `code` in a block of its own and returns the contract.
    fn deploy(&mut self, code: Vec<u8>) -> [u8; 32] {
        let hash = sha256(&code);
        let entry = self.entry(50_000_000, deploy(code));
        let t = self.block(&[entry]);
        assert_eq!(t.failed, 0, "the deployment");
        builtin::contract_id(&self.user.account, self.nonce - 1, &hash)
    }
}

const HEAD: &str = r#"
  (import "ksana" "storage_get" (func $storage_get (param i32 i32) (result i64)))
  (import "ksana" "storage_set" (func $storage_set (param i32 i32 i32 i32)))
  (import "ksana" "call_contract" (func $call_contract (param i32 i32 i32 i32 i32) (result i64)))
  (func (export "ksana_alloc") (param i32) (result i32) (i32.const 262144))
  ;; Borsh Ok(empty) at 200 000
  (func $ok (result i64)
    (i32.store8 (i32.const 200000) (i32.const 1))
    (i32.store (i32.const 200001) (i32.const 0))
    (i64.const 0x30d4000000005))
"#;

fn wasm(body: &str) -> Vec<u8> {
    wat::parse_str(format!("(module {HEAD} {body})")).unwrap()
}

/// A store: the arguments are five u64 (`write`, `start`, `count`, `value_len`, `modulus`). It writes `count` values
/// of `value_len` bytes under the 32-byte keys numbered from `start`, or reads `count` keys scattered below `modulus`.
fn store() -> Vec<u8> {
    wasm(
        r#"(memory (export "memory") 8)
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32) (local $i i64) (local $n i64)
    (if (i32.eq (i32.load (local.get $p)) (i32.const 4)) (then (return (call $ok)))) ;; init
    (local.set $a (i32.add (i32.add (local.get $p) (i32.const 8)) (i32.load (local.get $p))))
    (local.set $i (i64.load (i32.add (local.get $a) (i32.const 8))))
    (local.set $n (i64.add (local.get $i) (i64.load (i32.add (local.get $a) (i32.const 16)))))
    (if (i64.eqz (i64.load (local.get $a))) (then
      (loop $read
        (i64.store (i32.const 0) (i64.rem_u (i64.mul (local.get $i) (i64.const 2654435761)) (i64.load (i32.add (local.get $a) (i32.const 32)))))
        (drop (call $storage_get (i32.const 0) (i32.const 32)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br_if $read (i64.lt_u (local.get $i) (local.get $n)))))
    (else
      (loop $write
        (i64.store (i32.const 0) (local.get $i))
        (i64.store (i32.const 65536) (local.get $i))
        (call $storage_set (i32.const 0) (i32.const 32) (i32.const 65536) (i32.wrap_i64 (i64.load (i32.add (local.get $a) (i32.const 24)))))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br_if $write (i64.lt_u (local.get $i) (local.get $n))))))
    (call $ok))"#,
    )
}

fn store_args(write: bool, start: u64, count: u64, value_len: u64, modulus: u64) -> Vec<u8> {
    [u64::from(write), start, count, value_len, modulus]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn storage() {
    const PER_BLOCK: u64 = 9_300;
    const BLOCKS: u64 = 100;
    let mut b = Bench::new();
    let store = b.deploy(store());
    let start = Instant::now();
    let (mut execute, mut commit) = (Duration::ZERO, Duration::ZERO);
    for i in 0..BLOCKS {
        let args = store_args(true, i * PER_BLOCK, PER_BLOCK, 32, 0);
        let entry = b.entry(100_000_000, call(store, "x", args, vec![]));
        let t = b.block(&[entry]);
        assert_eq!(t.failed, 0);
        execute += t.execute;
        commit += t.commit;
        if (i + 1) % 20 == 0 {
            println!(
                "  with {} keys: root {:.0} ms, commit {:.0} ms",
                (i + 1) * PER_BLOCK,
                t.root.as_secs_f64() * 1e3,
                t.commit.as_secs_f64() * 1e3
            );
        }
    }
    let keys = BLOCKS * PER_BLOCK;
    println!(
        "populated {keys} keys in {BLOCKS} blocks in {:.1} s: execute {:.0} ms and commit {:.0} ms a block on average",
        start.elapsed().as_secs_f64(),
        execute.as_secs_f64() * 1e3 / BLOCKS as f64,
        commit.as_secs_f64() * 1e3 / BLOCKS as f64,
    );
    for round in 0..3 {
        let args = store_args(true, keys + round * PER_BLOCK, PER_BLOCK, 32, 0);
        let entry = b.entry(100_000_000, call(store, "x", args, vec![]));
        b.measure("storage_set of 9 300 new keys, 32 + 32 bytes", &[entry]);
    }
    for round in 0..2 {
        let args = store_args(true, round * 100_000, PER_BLOCK, 31, 0);
        let entry = b.entry(100_000_000, call(store, "x", args, vec![]));
        b.measure("storage_set over 9 300 existing keys", &[entry]);
    }
    for round in 0..3 {
        let args = store_args(false, round * 1_000_000, 47_000, 0, keys);
        let entry = b.entry(100_000_000, call(store, "x", args, vec![]));
        b.measure("storage_get of 47 000 existing keys", &[entry]);
    }
    for round in 0..2 {
        let args = store_args(true, 1 << 40 | round << 20, 149, 65_536, 0);
        let entry = b.entry(100_000_000, call(store, "x", args, vec![]));
        b.measure("storage_set of 149 new values of 64 KiB", &[entry]);
    }
    let args = store_args(false, 0, 47_000, 0, 1 << 60);
    let entry = b.entry(100_000_000, call(store, "x", args, vec![]));
    b.measure("storage_get of 47 000 missing keys", &[entry]);
}

/// A module of about `bytes` bytes that differs with `seed`: one function of branches, slow to compile for its size.
fn branchy(seed: u64, bytes: usize) -> Vec<u8> {
    let mut body =
        String::from("(memory (export \"memory\") 8)\n(func $f (param $x i64) (result i64)\n");
    for a in 0..bytes / 40 {
        let c = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(a as u64)
            | 0x4000_0000_0000_0000;
        body.push_str(&format!(
            "(if (i64.lt_u (local.get $x) (i64.const {c})) (then (local.set $x (i64.xor (local.get $x) (i64.const {c})))) (else (local.set $x (i64.add (local.get $x) (i64.const {c})))))\n"
        ));
    }
    body.push_str("(local.get $x))\n");
    body.push_str(r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $ok))"#);
    wasm(&body)
}

/// A module of about `bytes` bytes that differs with `seed`, slower to compile: one function of branches over
/// `locals` locals, each of them written in a branch and all of them read at the end.
fn crowded(seed: u64, locals: usize, bytes: usize) -> Vec<u8> {
    let arms = bytes.saturating_sub(locals * 4) / 28;
    let mut body =
        String::from("(memory (export \"memory\") 8)\n(func $f (param $x i64) (result i64)\n");
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
    body.push_str(r#"(func (export "ksana_call") (param i32 i32) (result i64) (call $ok))"#);
    wasm(&body)
}

/// Calls of more contracts than the compilation cache holds, in turn: each call compiles its contract again.
fn rotation(shape: &str, make: impl Fn(u64) -> Vec<u8>) {
    const CONTRACTS: u64 = 260;
    let mut b = Bench::new();
    let start = Instant::now();
    let contracts: Vec<[u8; 32]> = (0..CONTRACTS).map(|seed| b.deploy(make(seed))).collect();
    println!(
        "{shape}: deployed {CONTRACTS} contracts of {} bytes in {:.1} s",
        make(0).len(),
        start.elapsed().as_secs_f64()
    );
    let turn = |b: &mut Bench, of: usize| {
        b.fill(usize::MAX, 200_000, |i| {
            call(contracts[i % of], "x", vec![], vec![])
        })
    };
    let entries = turn(&mut b, contracts.len());
    b.measure(
        &format!("{shape}: calls of 260 contracts in turn"),
        &entries,
    );
    let entries = turn(&mut b, 200);
    b.measure(&format!("{shape}: calls of 200 of them in turn"), &entries);
    let entries = turn(&mut b, 200);
    b.measure(&format!("{shape}: the same calls, compiled"), &entries);
}

#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn calls_of_contracts_not_compiled() {
    rotation("branches", |seed| branchy(seed, 52_000));
}

#[test]
#[ignore = "a measurement of ten minutes: run in release on an idle machine"]
fn calls_of_contracts_slow_to_compile() {
    rotation("branches over 1 000 locals", |seed| {
        crowded(seed, 1_000, 60_000)
    });
}

/// Instances that write to every 4 KiB of the largest memory, 15 of them to a top-level call.
#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn instances_that_touch_their_memory() {
    let mut b = Bench::new();
    let toucher = b.deploy(
        wat::parse_str(
            r#"(module
  (memory (export "memory") 256)
  (func (export "ksana_alloc") (param i32) (result i32) (i32.const 65536))
  (func (export "ksana_call") (param i32 i32) (result i64)
    (local $a i32)
    (local.set $a (i32.const 131072))
    (loop $next
      (i32.store8 (local.get $a) (i32.const 1))
      (local.set $a (i32.add (local.get $a) (i32.const 4096)))
      (br_if $next (i32.lt_u (local.get $a) (i32.const 16777216))))
    (i32.store8 (i32.const 0) (i32.const 1))
    (i32.store (i32.const 1) (i32.const 0))
    (i64.const 5)))"#,
        )
        .unwrap(),
    );
    // the arguments are the callee and a u32, the number of calls; the input of each call is the method "x" without
    // arguments
    let driver = b.deploy(wasm(
        r#"(memory (export "memory") 8)
  (data (i32.const 1000) "\01\00\00\00x\00\00\00\00")
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32) (local $i i32)
    (if (i32.eq (i32.load (local.get $p)) (i32.const 4)) (then (return (call $ok)))) ;; init
    (local.set $a (i32.add (i32.add (local.get $p) (i32.const 8)) (i32.load (local.get $p))))
    (memory.copy (i32.const 2000) (local.get $a) (i32.const 32))
    (loop $again
      (drop (call $call_contract (i32.const 2000) (i32.const 1000) (i32.const 9) (i32.const 0) (i32.const 0)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $again (i32.lt_u (local.get $i) (i32.load (i32.add (local.get $a) (i32.const 32))))))
    (call $ok))"#,
    ));
    let calls = |callee: [u8; 32], count: u32| [&callee[..], &count.to_le_bytes()[..]].concat();
    let one = b.entry(5_000_000, call(driver, "x", calls(toucher, 15), vec![]));
    let t = b.measure("one call of 15 instances touching 16 MiB", &[one]);
    // the limits of gas of a block's ops add up to at most BLOCK_GAS_LIMIT
    let max_gas = t.gas + t.gas / 100;
    let most = usize::try_from(100_000_000 / max_gas).unwrap();
    for _ in 0..2 {
        let entries = b.fill(most, max_gas, |_| {
            call(driver, "x", calls(toucher, 15), vec![])
        });
        b.measure("a block of such calls", &entries);
    }
    let entries = b.fill(usize::MAX, 200_000, |_| call(toucher, "x", vec![], vec![]));
    b.measure("top-level calls touching 16 MiB, a block's data", &entries);

    // the largest memory declared and left alone: what a node pays that clears a memory it hands out again
    let idle = b.deploy(wasm(
        r#"(memory (export "memory") 256)
  (func (export "ksana_call") (param i32 i32) (result i64) (call $ok))"#,
    ));
    let one = b.entry(5_000_000, call(driver, "x", calls(idle, 15), vec![]));
    let t = b.measure("one call of 15 instances of 16 MiB left alone", &[one]);
    let max_gas = t.gas + t.gas / 100;
    let most = usize::try_from(100_000_000 / max_gas).unwrap();
    for _ in 0..2 {
        let entries = b.fill(most, max_gas, |_| {
            call(driver, "x", calls(idle, 15), vec![])
        });
        b.measure("a block of such calls", &entries);
    }

    let small = b.deploy(wasm(
        r#"(memory (export "memory") 8)
  (func (export "ksana_call") (param i32 i32) (result i64) (call $ok))"#,
    ));
    let one = b.entry(5_000_000, call(driver, "x", calls(small, 63), vec![]));
    let t = b.measure("one call of 63 instances of a small contract", &[one]);
    let max_gas = t.gas + t.gas / 100;
    let most = usize::try_from(100_000_000 / max_gas).unwrap();
    let entries = b.fill(most, max_gas, |_| {
        call(driver, "x", calls(small, 63), vec![])
    });
    b.measure("a block of such calls", &entries);
}

/// What an ordinary full block costs: as many transfers, and as many calls of a small contract, as its data holds.
#[test]
#[ignore = "a measurement: run in release on an idle machine"]
fn ordinary_blocks() {
    let mut b = Bench::new();
    let to = user("receiver").account;
    for _ in 0..2 {
        let entries = b.fill(usize::MAX, 100_000, |_| transfer(to, 1));
        b.measure("transfers, a block's data", &entries);
    }
    let small = b.deploy(wasm(
        r#"(memory (export "memory") 8)
  (func (export "ksana_call") (param i32 i32) (result i64) (call $ok))"#,
    ));
    for _ in 0..2 {
        let entries = b.fill(usize::MAX, 100_000, |_| call(small, "x", vec![], vec![]));
        b.measure("calls of a small contract, a block's data", &entries);
    }
}
