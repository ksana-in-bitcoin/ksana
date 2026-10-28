//! Static checks of the example contracts' wasm (self-check of ABI §1, §3, §5; the authoritative deploy checks are in
//! `ksana-vm`).
//!
//! - Fully validates the module with a feature set enabling only the proposals the ABI allows (mutable-globals,
//!   sign-extension, bulk-memory, multi-value, reference-types) and disabling floating point;
//! - additionally scans item by item: no floating point / SIMD in value types or instructions, instructions only from
//!   the allowed proposals;
//! - imports only from the `ksana` module, with names and signatures as in ABI §3; exports are exactly `memory`,
//!   `ksana_alloc`, `ksana_call`; exactly one own linear memory whose initial and maximum page counts do not exceed
//!   256;
//! - prints whether the static call graph (`call`, `ref.func`; `call_indirect` is treated as calling every function of
//!   the same type in the table, and the host functions `storage_get` / `call_contract` / `call_funds` / `native_tx` /
//!   `try_call_contract` / `delegate_call` are treated as calling back into `ksana_alloc`) has cycles, and the stack
//!   usage of the longest call chain estimated per ABI §5, for information only: since D-073 recursion is allowed and
//!   stack depth is metered at runtime.

use std::collections::{BTreeMap, BTreeSet};

use ksana_test_contracts::ALL;
use wasmparser::{
    ElementItems, ExternalKind, FuncType, Operator, OperatorsReader, Parser, Payload, RefType,
    TypeRef, ValType, ValidPayload, Validator, WasmFeatures,
};

const MAX_MEMORY_PAGES: u64 = 256;
const MAX_TABLE_ELEMENTS: u64 = 4096;
const STACK_BUDGET: u64 = 262_144;

/// The host functions of ABI §3: name, params, results.
const HOST_FUNCTIONS: &[(&str, &[ValType], &[ValType])] = {
    use ValType::{I32, I64};
    &[
        ("storage_get", &[I32, I32], &[I64]),
        ("storage_set", &[I32, I32, I32, I32], &[]),
        ("storage_remove", &[I32, I32], &[]),
        ("caller", &[I32], &[]),
        ("self_id", &[I32], &[]),
        ("block_height", &[], &[I64]),
        ("block_timestamp", &[], &[I64]),
        ("chain_id", &[], &[I64]),
        ("gas_left", &[], &[I64]),
        ("btc_anchor_height", &[], &[I64]),
        ("btc_header", &[I64, I32], &[]),
        ("asset_balance", &[I32, I32, I32], &[]),
        ("asset_transfer", &[I32, I32, I32], &[]),
        ("asset_issue", &[I32, I32, I32, I32, I32, I32], &[]),
        ("asset_burn", &[I32, I32], &[]),
        ("asset_create", &[I32, I32, I32, I32, I32, I32, I32], &[]),
        ("asset_freeze", &[I32, I32, I32], &[]),
        ("asset_allow", &[I32, I32, I32], &[]),
        ("asset_pause", &[I32, I32], &[]),
        ("asset_renounce", &[I32, I32], &[]),
        ("asset_policy", &[I32], &[I64]),
        ("asset_status", &[I32, I32], &[I32]),
        ("create_contract", &[I32, I32, I32, I32, I32, I32, I32], &[]),
        ("call_contract", &[I32, I32, I32, I32, I32], &[I64]),
        ("call_funds", &[], &[I64]),
        ("native_tx", &[], &[I64]),
        ("try_call_contract", &[I32, I32, I32, I32, I32, I64], &[I64]),
        ("delegate_call", &[I32, I32, I32], &[I64]),
        ("emit_event", &[I32, I32], &[]),
        ("sha256", &[I32, I32, I32], &[]),
        ("verify_schnorr", &[I32, I32, I32], &[I32]),
    ]
};

/// Host functions that return a packed buffer, for which the VM calls back into `ksana_alloc` to allocate memory.
const CALLS_BACK_ALLOC: &[&str] = &[
    "storage_get",
    "call_contract",
    "call_funds",
    "native_tx",
    "try_call_contract",
    "delegate_call",
];

/// The proposals allowed by ABI §1; excludes floating point (`FLOATS`), SIMD, non-trapping float conversions, etc.
fn allowed_features() -> WasmFeatures {
    WasmFeatures::MUTABLE_GLOBAL
        | WasmFeatures::SIGN_EXTENSION
        | WasmFeatures::BULK_MEMORY
        | WasmFeatures::MULTI_VALUE
        | WasmFeatures::REFERENCE_TYPES
}

macro_rules! define_classify {
    ($( @$proposal:ident $op:ident $({ $($arg:ident: $argty:ty),* })? => $visit:ident ($($ann:tt)*))*) => {
        /// Instruction name and the proposal it belongs to.
        fn classify(op: &Operator<'_>) -> (&'static str, &'static str) {
            match op {
                $( Operator::$op { .. } => (stringify!($op), stringify!($proposal)), )*
                _ => ("<unknown>", "<unknown>"),
            }
        }
    };
}
wasmparser::for_each_operator!(define_classify);

fn is_float_or_simd_type(ty: ValType) -> bool {
    matches!(ty, ValType::F32 | ValType::F64 | ValType::V128)
}

#[derive(Default)]
struct Module {
    types: Vec<FuncType>,
    /// Type index of every function (imports first, then defined ones).
    func_types: Vec<u32>,
    imports: Vec<(String, String, TypeRef)>,
    exports: BTreeMap<String, (ExternalKind, u32)>,
    memories: Vec<wasmparser::MemoryType>,
    tables: Vec<wasmparser::TableType>,
    /// Functions appearing in element segments (callable via `call_indirect`).
    table_functions: BTreeSet<u32>,
    /// Direct call edges (`call`, `ref.func`) of every defined function and the types of its `call_indirect`s.
    calls: BTreeMap<u32, BTreeSet<u32>>,
    indirect_types: BTreeMap<u32, BTreeSet<u32>>,
    /// ABI §5 stack frame estimate of every defined function.
    frames: BTreeMap<u32, u64>,
    /// Descriptions of rule violations.
    problems: Vec<String>,
}

impl Module {
    fn func_type(&self, func: u32) -> &FuncType {
        &self.types[self.func_types[func as usize] as usize]
    }

    fn num_imported_funcs(&self) -> u32 {
        self.imports
            .iter()
            .filter(|(_, _, ty)| matches!(ty, TypeRef::Func(_)))
            .count() as u32
    }
}

fn analyze(name: &str, bytes: &[u8]) -> Module {
    let mut module = Module::default();
    let mut validator = Validator::new_with_features(allowed_features());
    let mut next_defined_func = 0u32;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.unwrap_or_else(|e| panic!("{name}: parse error: {e}"));
        let valid = validator
            .payload(&payload)
            .unwrap_or_else(|e| panic!("{name}: invalid under the ABI feature set: {e}"));
        match &payload {
            Payload::TypeSection(reader) => {
                for ty in reader.clone().into_iter_err_on_gc_types() {
                    module.types.push(ty.unwrap());
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader.clone().into_imports() {
                    let import = import.unwrap();
                    if let TypeRef::Func(ty) = import.ty {
                        module.func_types.push(ty);
                    }
                    module.imports.push((
                        import.module.to_string(),
                        import.name.to_string(),
                        import.ty,
                    ));
                }
            }
            Payload::FunctionSection(reader) => {
                for ty in reader.clone() {
                    module.func_types.push(ty.unwrap());
                }
            }
            Payload::TableSection(reader) => {
                for table in reader.clone() {
                    module.tables.push(table.unwrap().ty);
                }
            }
            Payload::MemorySection(reader) => {
                for memory in reader.clone() {
                    module.memories.push(memory.unwrap());
                }
            }
            Payload::GlobalSection(reader) => {
                for global in reader.clone() {
                    let global = global.unwrap();
                    if is_float_or_simd_type(global.ty.content_type) {
                        module
                            .problems
                            .push(format!("float/SIMD global {:?}", global.ty));
                    }
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader.clone() {
                    let export = export.unwrap();
                    module
                        .exports
                        .insert(export.name.to_string(), (export.kind, export.index));
                }
            }
            Payload::ElementSection(reader) => {
                for element in reader.clone() {
                    match element.unwrap().items {
                        ElementItems::Functions(funcs) => {
                            for f in funcs {
                                module.table_functions.insert(f.unwrap());
                            }
                        }
                        ElementItems::Expressions(_, exprs) => {
                            for expr in exprs {
                                let mut ops = expr.unwrap().get_operators_reader();
                                while !ops.eof() {
                                    if let Operator::RefFunc { function_index } =
                                        ops.read().unwrap()
                                    {
                                        module.table_functions.insert(function_index);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        if let ValidPayload::Func(to_validate, body) = valid {
            let func = module.num_imported_funcs() + next_defined_func;
            next_defined_func += 1;
            let mut fv = to_validate.into_validator(Default::default());
            let mut reader = body.get_binary_reader();
            fv.read_locals(&mut reader).unwrap();
            let params = module.func_type(func).params().len() as u64;
            let mut locals = 0u64;
            for local in body.get_locals_reader().unwrap() {
                let (count, ty) = local.unwrap();
                locals += u64::from(count);
                if is_float_or_simd_type(ty) {
                    module
                        .problems
                        .push(format!("f{func}: float/SIMD local {ty:?}"));
                }
            }
            let mut max_height = 0u64;
            let mut calls = BTreeSet::new();
            let mut indirect = BTreeSet::new();
            let mut ops = OperatorsReader::new(reader);
            while !ops.eof() {
                let (op, offset) = ops.read_with_offset().unwrap();
                fv.op(offset, &op)
                    .unwrap_or_else(|e| panic!("{name}: f{func}: invalid operator: {e}"));
                max_height = max_height.max(u64::from(fv.operand_stack_height()));
                let (op_name, proposal) = classify(&op);
                if op_name.contains("F32") || op_name.contains("F64") {
                    module
                        .problems
                        .push(format!("f{func}: float instruction {op_name}"));
                }
                if op_name.contains("V128") || proposal.contains("simd") {
                    module
                        .problems
                        .push(format!("f{func}: SIMD instruction {op_name}"));
                }
                if !matches!(
                    proposal,
                    "mvp" | "sign_extension" | "bulk_memory" | "reference_types"
                ) {
                    module
                        .problems
                        .push(format!("f{func}: {op_name} from proposal {proposal}"));
                }
                match op {
                    Operator::Call { function_index } | Operator::RefFunc { function_index } => {
                        calls.insert(function_index);
                    }
                    Operator::CallIndirect { type_index, .. } => {
                        indirect.insert(type_index);
                    }
                    Operator::TypedSelect { ty } if is_float_or_simd_type(ty) => {
                        module.problems.push(format!("f{func}: float/SIMD select"));
                    }
                    Operator::Block { blockty }
                    | Operator::Loop { blockty }
                    | Operator::If { blockty } => {
                        if let wasmparser::BlockType::Type(ty) = blockty {
                            if is_float_or_simd_type(ty) {
                                module
                                    .problems
                                    .push(format!("f{func}: float/SIMD block type"));
                            }
                        }
                    }
                    _ => {}
                }
            }
            ops.finish().unwrap();
            module.calls.insert(func, calls);
            module.indirect_types.insert(func, indirect);
            module
                .frames
                .insert(func, 64 + 16 * (params + locals + max_height));
        }
    }
    for (i, ty) in module.types.iter().enumerate() {
        if ty
            .params()
            .iter()
            .chain(ty.results())
            .any(|t| is_float_or_simd_type(*t))
        {
            module
                .problems
                .push(format!("float/SIMD in type {i}: {ty}"));
        }
    }
    module
}

/// Edges of the call graph (ABI §5): `call` / `ref.func`; `call_indirect` points at every function in the table with a
/// structurally identical type; host functions that call back into `ksana_alloc` point at `ksana_alloc`.
fn call_graph(module: &Module) -> BTreeMap<u32, BTreeSet<u32>> {
    let num_funcs = module.func_types.len() as u32;
    let mut graph: BTreeMap<u32, BTreeSet<u32>> =
        (0..num_funcs).map(|f| (f, BTreeSet::new())).collect();
    for (func, callees) in &module.calls {
        graph.get_mut(func).unwrap().extend(callees);
    }
    for (func, types) in &module.indirect_types {
        for ty in types {
            let wanted = &module.types[*ty as usize];
            for target in &module.table_functions {
                if module.func_type(*target) == wanted {
                    graph.get_mut(func).unwrap().insert(*target);
                }
            }
        }
    }
    let alloc = module.exports.get("ksana_alloc").map(|(_, index)| *index);
    let mut import_index = 0u32;
    for (_, name, ty) in &module.imports {
        if let TypeRef::Func(_) = ty {
            if let (true, Some(alloc)) = (CALLS_BACK_ALLOC.contains(&name.as_str()), alloc) {
                graph.get_mut(&import_index).unwrap().insert(alloc);
            }
            import_index += 1;
        }
    }
    graph
}

/// Kahn's topological sort: returns the topological order when acyclic, otherwise the functions on (and depending on)
/// a cycle.
fn topological_order(graph: &BTreeMap<u32, BTreeSet<u32>>) -> Result<Vec<u32>, Vec<u32>> {
    let mut indegree: BTreeMap<u32, usize> = graph.keys().map(|f| (*f, 0)).collect();
    for callees in graph.values() {
        for callee in callees {
            *indegree.get_mut(callee).unwrap() += 1;
        }
    }
    let mut ready: Vec<u32> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(f, _)| *f)
        .collect();
    let mut order = Vec::new();
    while let Some(func) = ready.pop() {
        order.push(func);
        for callee in &graph[&func] {
            let d = indegree.get_mut(callee).unwrap();
            *d -= 1;
            if *d == 0 {
                ready.push(*callee);
            }
        }
    }
    if order.len() == graph.len() {
        Ok(order)
    } else {
        Err(indegree
            .into_iter()
            .filter(|(_, d)| *d > 0)
            .map(|(f, _)| f)
            .collect())
    }
}

/// Estimated stack usage of the longest call chain starting from each function (host function frames count as 0).
fn max_stack_usage(module: &Module, graph: &BTreeMap<u32, BTreeSet<u32>>, order: &[u32]) -> u64 {
    let mut usage: BTreeMap<u32, u64> = BTreeMap::new();
    // Callers come first in the topological order; processing it in reverse ensures callees are computed first.
    for func in order.iter().rev() {
        let own = module.frames.get(func).copied().unwrap_or(0);
        let deepest = graph[func].iter().map(|c| usage[c]).max().unwrap_or(0);
        usage.insert(*func, own + deepest);
    }
    usage.values().copied().max().unwrap_or(0)
}

#[test]
fn modules_follow_the_abi() {
    for (name, bytes) in ALL {
        assert_eq!(&bytes[..4], b"\0asm", "{name}: not a wasm module");
        let module = analyze(name, bytes);
        assert!(module.problems.is_empty(), "{name}: {:#?}", module.problems);

        // Imports: only functions of the `ksana` module, with names and signatures as in ABI §3.
        for (import_module, import_name, ty) in &module.imports {
            assert_eq!(
                import_module, "ksana",
                "{name}: import from {import_module}"
            );
            let TypeRef::Func(type_index) = ty else {
                panic!("{name}: non-function import {import_name}: {ty:?}");
            };
            let (_, params, results) = HOST_FUNCTIONS
                .iter()
                .find(|(n, _, _)| n == import_name)
                .unwrap_or_else(|| panic!("{name}: unknown host function {import_name}"));
            let ty = &module.types[*type_index as usize];
            assert_eq!(ty.params(), *params, "{name}: {import_name} params");
            assert_eq!(ty.results(), *results, "{name}: {import_name} results");
        }

        // Exports: exactly memory, ksana_alloc, ksana_call.
        let exported: Vec<&str> = module.exports.keys().map(String::as_str).collect();
        assert_eq!(
            exported,
            ["ksana_alloc", "ksana_call", "memory"],
            "{name}: exports"
        );
        assert!(matches!(
            module.exports["memory"],
            (ExternalKind::Memory, 0)
        ));
        let (kind, alloc) = module.exports["ksana_alloc"];
        assert_eq!(kind, ExternalKind::Func);
        assert_eq!(module.func_type(alloc).params(), [ValType::I32]);
        assert_eq!(module.func_type(alloc).results(), [ValType::I32]);
        let (kind, call) = module.exports["ksana_call"];
        assert_eq!(kind, ExternalKind::Func);
        assert_eq!(
            module.func_type(call).params(),
            [ValType::I32, ValType::I32]
        );
        assert_eq!(module.func_type(call).results(), [ValType::I64]);

        // Exactly one own linear memory (the import check already rules out imported memories), at most 256 pages.
        assert_eq!(module.memories.len(), 1, "{name}: memories");
        let memory = module.memories[0];
        assert!(!memory.memory64 && !memory.shared, "{name}: {memory:?}");
        assert!(memory.initial <= MAX_MEMORY_PAGES, "{name}: {memory:?}");
        assert!(
            memory.maximum.is_some_and(|max| max <= MAX_MEMORY_PAGES),
            "{name}: memory maximum must be declared and <= 256 pages: {memory:?}"
        );

        // At most one funcref table.
        assert!(module.tables.len() <= 1, "{name}: tables");
        for table in &module.tables {
            assert_eq!(table.element_type, RefType::FUNCREF, "{name}: {table:?}");
            assert!(table.initial <= MAX_TABLE_ELEMENTS, "{name}: {table:?}");
            assert!(table.maximum.is_none_or(|max| max <= MAX_TABLE_ELEMENTS));
        }

        // The call graph and estimated stack usage are for information only (D-073: recursion is allowed, stack depth
        // is metered at runtime, exceeding STACK_BUDGET traps).
        let graph = call_graph(&module);
        let stack = match topological_order(&graph) {
            Ok(order) => {
                let usage = max_stack_usage(&module, &graph, &order);
                format!("max estimated stack {usage} of {STACK_BUDGET} bytes")
            }
            Err(cycle) => format!("recursive through {} functions", cycle.len()),
        };
        println!(
            "{name}: {} bytes, {} functions, {} table entries, {stack}",
            bytes.len(),
            module.func_types.len(),
            module.table_functions.len()
        );
    }
}

/// Self-check: the analyzer really detects floating-point instructions, recursion, indirect recursion and illegal
/// imports (hand-written minimal modules).
#[test]
fn checker_detects_violations() {
    // (module (func (param f32))): floating point fails validation under the ABI feature set.
    let float_module = [
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version
        0x01, 0x05, 0x01, 0x60, 0x01, 0x7d, 0x00, // type: (func (param f32))
        0x03, 0x02, 0x01, 0x00, // function 0 : type 0
        0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b, // code: empty body
    ];
    assert!(Validator::new_with_features(allowed_features())
        .validate_all(&float_module)
        .is_err());
    assert!(Validator::new().validate_all(&float_module).is_ok());

    // (func $f call $f): direct recursion.
    let direct = [
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, //
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: (func)
        0x03, 0x02, 0x01, 0x00, // function 0
        0x0a, 0x06, 0x01, 0x04, 0x00, 0x10, 0x00, 0x0b, // body: call 0
    ];
    let module = analyze("direct", &direct);
    assert!(topological_order(&call_graph(&module)).is_err());

    // $f is in the table and $f executes call_indirect (type 0): indirect recursion.
    let indirect = [
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, //
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type 0: (func)
        0x03, 0x02, 0x01, 0x00, // function 0 : type 0
        0x04, 0x04, 0x01, 0x70, 0x00, 0x01, // table: funcref min 1
        0x09, 0x07, 0x01, 0x00, 0x41, 0x00, 0x0b, 0x01, 0x00, // elem: offset 0, [f0]
        0x0a, 0x09, 0x01, 0x07, 0x00, 0x41, 0x00, 0x11, 0x00, 0x00,
        0x0b, // body: i32.const 0; call_indirect 0 0
    ];
    let module = analyze("indirect", &indirect);
    assert_eq!(module.table_functions, BTreeSet::from([0]));
    assert!(topological_order(&call_graph(&module)).is_err());
}
