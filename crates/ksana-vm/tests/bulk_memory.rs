//! The bulk memory instructions a contract may use (`memory.copy`, `memory.fill`; the first audit's F-32): they write
//! what the instruction says, also when the ranges overlap; a range ending exactly at the end of memory is taken and
//! one byte past it traps, with nothing written; and the same call costs the same gas on both engine configurations.

mod common;

use common::{id, module, MockHost};
use ksana_primitives::DEVNET;
use ksana_vm::{CallOutcome, CallRequest, FailureReason, Vm};

const GAS: u64 = 1_000_000_000;

fn run(vm: &Vm, body: &str, args: &[u8]) -> Result<(Vec<u8>, u64), FailureReason> {
    let mut host = MockHost::default();
    host.deploy(id(1), module(body));
    let outcome = vm
        .call(
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
        .expect("no VM internal error expected");
    match outcome {
        CallOutcome::Success { output, gas_used } => Ok((output, gas_used)),
        CallOutcome::Failure(reason) => Err(reason),
    }
}

/// Writes the bytes 1 to 16 at 1000, then runs `memory.copy` or `memory.fill` with the three u32 arguments (for a copy:
/// destination, source, length; for a fill: destination, value, length), each an offset from 1000 unless `FROM_END`
/// is set in it, which counts it back from the end of memory instead. Returns the 32 bytes from 1000.
fn body(instruction: &str) -> String {
    format!(
        r#"
  (func $at (param $v i32) (result i32)
    (if (result i32) (i32.and (local.get $v) (i32.const 0x40000000))
      (then (i32.sub (i32.mul (memory.size) (i32.const 65536))
                     (i32.and (local.get $v) (i32.const 0x3fffffff))))
      (else (i32.add (i32.const 1000) (local.get $v)))))
  (func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
    (local $a i32) (local $i i32)
    (local.set $a (call $args_ptr (local.get $p)))
    (block $done
      (loop $next
        (br_if $done (i32.eq (local.get $i) (i32.const 16)))
        (i32.store8 (i32.add (i32.const 1000) (local.get $i)) (i32.add (local.get $i) (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    {instruction}
    (call $ok (i32.const 1000) (i32.const 32)))
"#
    )
}

const COPY: &str = "(memory.copy (call $at (i32.load (local.get $a))) \
                    (call $at (i32.load (i32.add (local.get $a) (i32.const 4)))) \
                    (i32.load (i32.add (local.get $a) (i32.const 8))))";
const FILL: &str = "(memory.fill (call $at (i32.load (local.get $a))) \
                    (i32.load (i32.add (local.get $a) (i32.const 4))) \
                    (i32.load (i32.add (local.get $a) (i32.const 8))))";
const FROM_END: u32 = 0x4000_0000;

fn args(a: u32, b: u32, c: u32) -> Vec<u8> {
    [a, b, c].iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn start() -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    for (i, byte) in bytes.iter_mut().take(16).enumerate() {
        *byte = i as u8 + 1;
    }
    bytes
}

#[test]
fn memory_copy_moves_the_bytes_also_when_the_ranges_overlap() {
    let vm = Vm::new(&DEVNET).unwrap();
    for (dst, src, len) in [(20, 0, 8), (4, 0, 12), (0, 4, 12), (3, 3, 5), (16, 0, 0)] {
        let mut expected = start();
        expected.copy_within(src..src + len, dst);
        let (output, _) = run(&vm, &body(COPY), &args(dst as u32, src as u32, len as u32)).unwrap();
        assert_eq!(output, expected, "copy {dst} <- {src}, {len} bytes");
    }
}

#[test]
fn memory_fill_writes_the_low_byte_of_the_value_over_the_range_alone() {
    let vm = Vm::new(&DEVNET).unwrap();
    for (dst, value, len) in [
        (2usize, 0xabu32, 5usize),
        (0, 0x1ff, 16),
        (30, 7, 2),
        (8, 9, 0),
    ] {
        let mut expected = start();
        expected[dst..dst + len].fill(value as u8);
        let (output, _) = run(&vm, &body(FILL), &args(dst as u32, value, len as u32)).unwrap();
        assert_eq!(output, expected, "fill {dst}, {len} bytes of {value}");
    }
}

#[test]
fn a_range_up_to_the_end_of_memory_is_taken_and_one_byte_past_it_traps() {
    let a = Vm::with_parallel_compilation(&DEVNET, true).unwrap();
    let b = Vm::with_parallel_compilation(&DEVNET, false).unwrap();
    let cases = [
        // the destination ends at the end of memory, and one byte past it
        (COPY, args(FROM_END | 8, 0, 8), true),
        (COPY, args(FROM_END | 7, 0, 8), false),
        // the source does
        (COPY, args(16, FROM_END | 8, 8), true),
        (COPY, args(16, FROM_END | 7, 8), false),
        // no bytes at the end of memory, and no bytes one past it
        (COPY, args(FROM_END, 0, 0), true),
        (COPY, args(FROM_END - 1, 0, 0), false),
        (FILL, args(FROM_END | 8, 0xee, 8), true),
        (FILL, args(FROM_END | 7, 0xee, 8), false),
        (FILL, args(FROM_END, 0xee, 0), true),
        (FILL, args(FROM_END - 1, 0xee, 0), false),
    ];
    for (instruction, args, taken) in cases {
        let first = run(&a, &body(instruction), &args);
        assert_eq!(first.is_ok(), taken, "{instruction} {args:?}: {first:?}");
        // the same call again, and on the other engine configuration: the same answer and the same gas
        assert_eq!(run(&a, &body(instruction), &args), first);
        assert_eq!(run(&b, &body(instruction), &args), first);
    }
}
