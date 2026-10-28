//! Deployment-time static checks (docs/ABI.md sections 1 and 5): the M1 acceptance item "a contract containing float
//! instructions is rejected at deployment", among others.

mod common;

use common::{module, raw};
use ksana_primitives::DEVNET;
use ksana_vm::{CodeError, Vm};

fn vm() -> Vm {
    Vm::new(&DEVNET).unwrap()
}

const OK_ENTRY: &str = r#"
  (func (export "ksana_call") (param i32 i32) (result i64)
    (call $ok (i32.const 0) (i32.const 0)))
"#;

#[test]
fn minimal_contract_is_accepted() {
    vm().validate_code(&module(OK_ENTRY)).unwrap();
}

fn assert_invalid(code: &[u8]) {
    match vm().validate_code(code) {
        Err(CodeError::Invalid(_)) => {}
        other => panic!("should be rejected as invalid bytecode, got {other:?}"),
    }
}

#[test]
fn float_instructions_are_rejected() {
    // f32 arithmetic
    assert_invalid(&module(&format!(
        "{OK_ENTRY} (func $f (result i32) (i32.trunc_f32_s (f32.add (f32.const 1) (f32.const 2))))"
    )));
    // f64 constant
    assert_invalid(&module(&format!(
        "{OK_ENTRY} (func $g (drop (f64.const 1.5)))"
    )));
    // float local
    assert_invalid(&module(&format!("{OK_ENTRY} (func $h (local f64))")));
    // float parameter
    assert_invalid(&module(&format!("{OK_ENTRY} (func $k (param f32))")));
    // float global
    assert_invalid(&module(&format!(
        "{OK_ENTRY} (global $x (mut f64) (f64.const 0))"
    )));
    // float load
    assert_invalid(&module(&format!(
        "{OK_ENTRY} (func $l (result i32) (i32.trunc_f64_u (f64.load (i32.const 0))))"
    )));
    // non-trapping float conversion (nontrapping-fptoint)
    assert_invalid(&module(&format!(
        "{OK_ENTRY} (func $m (result i32) (i32.trunc_sat_f32_s (f32.const 1)))"
    )));
    // reinterpreting between i32 and float bit patterns
    assert_invalid(&module(&format!(
        "{OK_ENTRY} (func $n (result i32) (i32.reinterpret_f32 (f32.const 1)))"
    )));
}

#[test]
fn simd_threads_and_other_proposals_are_rejected() {
    // SIMD
    assert_invalid(&module(&format!(
        "{OK_ENTRY} (func $s (drop (v128.const i32x4 0 0 0 0)))"
    )));
    // atomics and shared memory
    assert_invalid(&raw(
        r#"(module (memory 1 1 shared) (func (drop (i32.atomic.load (i32.const 0)))))"#,
    ));
    // tail call
    assert_invalid(&module(&format!("{OK_ENTRY} (func $t (return_call $t))")));
    // multi-memory
    assert_invalid(&module(&format!("{OK_ENTRY} (memory 1)")));
}

#[test]
fn imports_are_restricted() {
    let with_import = |import: &str| {
        raw(&format!(
            "(module {import} {} {} {OK_ENTRY})",
            common::IMPORTS,
            common::RUNTIME
        ))
    };
    let other_module = with_import(r#"(import "env" "abort" (func $abort))"#);
    assert!(matches!(
        vm().validate_code(&other_module),
        Err(CodeError::Import(_))
    ));
    let unknown_name = with_import(r#"(import "ksana" "random" (func $r (result i64)))"#);
    assert!(matches!(
        vm().validate_code(&unknown_name),
        Err(CodeError::Import(_))
    ));
    let wrong_signature = raw(&format!(
        r#"(module
          (import "ksana" "block_height" (func $bh (result i32)))
          {} {OK_ENTRY})"#,
        common::RUNTIME
    ));
    assert!(matches!(
        vm().validate_code(&wrong_signature),
        Err(CodeError::Import(_))
    ));
    let imported_memory = raw(r#"(module
          (import "ksana" "memory" (memory 1))
          (func (export "ksana_alloc") (param i32) (result i32) (i32.const 0))
          (func (export "ksana_call") (param i32 i32) (result i64) (i64.const 0)))"#);
    assert!(matches!(
        vm().validate_code(&imported_memory),
        Err(CodeError::Import(_))
    ));
}

#[test]
fn required_exports_are_checked() {
    let missing_call = module("");
    assert!(matches!(
        vm().validate_code(&missing_call),
        Err(CodeError::Export(_))
    ));
    let wrong_type =
        module(r#"(func (export "ksana_call") (param i32 i32) (result i32) (i32.const 0))"#);
    assert!(matches!(
        vm().validate_code(&wrong_type),
        Err(CodeError::Export(_))
    ));
    let no_memory_export = raw(r#"(module (memory 1)
          (func (export "ksana_alloc") (param i32) (result i32) (i32.const 0))
          (func (export "ksana_call") (param i32 i32) (result i64) (i64.const 0)))"#);
    assert!(matches!(
        vm().validate_code(&no_memory_export),
        Err(CodeError::Export(_))
    ));
}

#[test]
fn memory_and_table_limits() {
    let too_big = raw(r#"(module (memory (export "memory") 257)
          (func (export "ksana_alloc") (param i32) (result i32) (i32.const 0))
          (func (export "ksana_call") (param i32 i32) (result i64) (i64.const 0)))"#);
    assert!(matches!(
        vm().validate_code(&too_big),
        Err(CodeError::Memory(_))
    ));
    let no_memory = raw(r#"(module
          (func (export "ksana_alloc") (param i32) (result i32) (i32.const 0))
          (func (export "ksana_call") (param i32 i32) (result i64) (i64.const 0)))"#);
    assert!(matches!(
        vm().validate_code(&no_memory),
        Err(CodeError::Memory(_))
    ));
    let unbounded_table = module(&format!("{OK_ENTRY} (table 1 funcref)"));
    assert!(matches!(
        vm().validate_code(&unbounded_table),
        Err(CodeError::Table(_))
    ));
    let huge_table = module(&format!("{OK_ENTRY} (table 1 5000 funcref)"));
    assert!(matches!(
        vm().validate_code(&huge_table),
        Err(CodeError::Table(_))
    ));
    let bounded_table = module(&format!("{OK_ENTRY} (table 1 16 funcref)"));
    vm().validate_code(&bounded_table).unwrap();
}

#[test]
fn recursion_and_large_frames_are_accepted_at_deploy() {
    // D-073: the call graph is no longer required to be acyclic; the stack depth is metered at runtime
    // (see tests/stack.rs)
    let direct = module(&format!("{OK_ENTRY} (func $r (call $r))"));
    vm().validate_code(&direct).unwrap();
    let mutual = module(&format!(
        "{OK_ENTRY} (func $a (call $b)) (func $b (call $a))"
    ));
    vm().validate_code(&mutual).unwrap();
    let indirect = module(&format!(
        r#"{OK_ENTRY}
        (type $void (func))
        (table 1 1 funcref)
        (elem (i32.const 0) $f)
        (func $f (type $void) (call_indirect (type $void) (i32.const 0)))"#
    ));
    vm().validate_code(&indirect).unwrap();
    // a function whose frame estimate exceeds the budget can also be deployed: calling it traps at the entry
    let huge_frame = module(&format!(
        "{OK_ENTRY} (func $big (local i64) (local i32 i32) {})",
        "(local i64)".repeat(19_998)
    ));
    vm().validate_code(&huge_frame).unwrap();
}

#[test]
fn garbage_is_rejected() {
    assert_invalid(b"not wasm");
    assert_invalid(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0xFF]);
}
