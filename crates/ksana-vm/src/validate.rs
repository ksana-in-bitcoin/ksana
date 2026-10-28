//! Deployment-time static checks (docs/ABI.md sections 1 and 5, DECISIONS D-014, D-073).
//!
//! First the bytecode is fully validated with `wasmparser` under a restricted feature set (no floats, SIMD, threads,
//! multi-memory, etc.), then the imports, exports, memory and tables are checked, and a frame estimate is computed
//! for each function (for runtime stack metering, see [`crate::instrument`]).
//! Recursion and indirect calls are no longer restricted (D-073): the stack depth is metered deterministically at
//! runtime.

use std::collections::BTreeMap;

use wasmparser::{
    CompositeInnerType, ExternalKind, FuncToValidate, FuncValidatorAllocations, FunctionBody,
    Imports, Parser, Payload, RefType, TypeRef, ValType, ValidPayload, Validator,
    ValidatorResources, WasmFeatures,
};

pub(crate) use crate::limits::STACK_BUDGET;
use crate::limits::{FRAME_BASE_BYTES, FRAME_SLOT_BYTES, MAX_TABLE_ELEMENTS, WASM_PAGE_BYTES};

/// The allowed wasm features: WebAssembly 1.0 (without floats) + mutable-globals, sign-extension, reference-types,
/// multi-value, bulk-memory. `FLOATS` is not enabled, so any float type or instruction fails validation; consistent
/// with wasmtime (without the `gc` compile feature), `GC_TYPES` is not included.
pub const FEATURES: WasmFeatures = WasmFeatures::MUTABLE_GLOBAL
    .union(WasmFeatures::SIGN_EXTENSION)
    .union(WasmFeatures::REFERENCE_TYPES)
    .union(WasmFeatures::MULTI_VALUE)
    .union(WasmFeatures::BULK_MEMORY);

/// The import module name.
pub const HOST_MODULE: &str = "ksana";

use ValType::{I32, I64};

/// Host functions: name, parameter types, result types (docs/ABI.md section 3).
pub const HOST_FUNCTIONS: &[(&str, &[ValType], &[ValType])] = &[
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
];

/// Required exported entry point: `ksana_alloc: (i32) -> i32`.
pub const EXPORT_ALLOC: &str = "ksana_alloc";
/// Required exported entry point: `ksana_call: (i32, i32) -> i64`.
pub const EXPORT_CALL: &str = "ksana_call";
/// The required exported linear memory.
pub const EXPORT_MEMORY: &str = "memory";

/// The reason the deployment check failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodeError {
    /// The bytecode is invalid or uses a disallowed feature (including any float instruction).
    #[error("wasm validation failed: {0}")]
    Invalid(String),
    /// A disallowed import.
    #[error("import not allowed: {0}")]
    Import(String),
    /// The linear memory does not meet the requirements.
    #[error("linear memory does not meet the requirements: {0}")]
    Memory(String),
    /// A table does not meet the requirements.
    #[error("table does not meet the requirements: {0}")]
    Table(String),
    /// A required export is missing or has the wrong type.
    #[error("export does not meet the requirements: {0}")]
    Export(String),
}

fn invalid(e: impl std::fmt::Display) -> CodeError {
    CodeError::Invalid(e.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Sig {
    params: Vec<ValType>,
    results: Vec<ValType>,
}

/// The analysis result of a function body.
#[derive(Default)]
struct BodyInfo {
    /// Frame estimate (bytes).
    frame: u64,
}

#[derive(Default)]
struct Module {
    types: Vec<Option<Sig>>,
    /// The type index of every function (imports first).
    func_types: Vec<u32>,
    memories: u32,
    tables: u32,
    exports: BTreeMap<String, (ExternalKind, u32)>,
    bodies: Vec<BodyInfo>,
}

impl Module {
    fn sig_of_func(&self, func: u32) -> Option<&Sig> {
        let ty = *self.func_types.get(usize::try_from(func).ok()?)?;
        self.types.get(usize::try_from(ty).ok()?)?.as_ref()
    }
}

fn check_import(module: &str, name: &str, ty: TypeRef, m: &mut Module) -> Result<(), CodeError> {
    if module != HOST_MODULE {
        return Err(CodeError::Import(format!("{module}.{name}")));
    }
    let type_index = match ty {
        TypeRef::Func(i) | TypeRef::FuncExact(i) => i,
        _ => {
            return Err(CodeError::Import(format!(
                "{module}.{name} is not a function"
            )))
        }
    };
    let (_, params, results) = HOST_FUNCTIONS
        .iter()
        .find(|(n, _, _)| *n == name)
        .ok_or_else(|| CodeError::Import(format!("{module}.{name}")))?;
    let sig = m
        .types
        .get(usize::try_from(type_index).map_err(invalid)?)
        .and_then(Option::as_ref)
        .ok_or_else(|| CodeError::Import(format!("type of {name} does not exist")))?;
    if sig.params != *params || sig.results != *results {
        return Err(CodeError::Import(format!(
            "signature of {name} does not match"
        )));
    }
    m.func_types.push(type_index);
    Ok(())
}

/// Validates and analyzes one function body: returns the frame estimate.
fn analyze_body(
    to_validate: FuncToValidate<ValidatorResources>,
    body: &FunctionBody<'_>,
    params: u64,
) -> Result<BodyInfo, CodeError> {
    // first pass: wasmparser's standard full validation.
    let mut full = FuncToValidate {
        resources: to_validate.resources.clone(),
        index: to_validate.index,
        ty: to_validate.ty,
        features: to_validate.features,
    }
    .into_validator(FuncValidatorAllocations::default());
    full.validate(body).map_err(invalid)?;

    // second pass: drive the same validator instruction by instruction, recording the maximum operand stack height.
    let mut fv = to_validate.into_validator(FuncValidatorAllocations::default());
    let mut locals: u64 = 0;
    let mut locals_reader = body.get_locals_reader().map_err(invalid)?;
    for _ in 0..locals_reader.get_count() {
        let offset = locals_reader.original_position();
        let (count, ty) = locals_reader.read().map_err(invalid)?;
        fv.define_locals(offset, count, ty).map_err(invalid)?;
        locals = locals.saturating_add(u64::from(count));
    }
    let mut info = BodyInfo::default();
    let mut max_height: u64 = 0;
    let mut ops = body.get_operators_reader().map_err(invalid)?;
    while !ops.eof() {
        let (op, offset) = ops.read_with_offset().map_err(invalid)?;
        fv.op(offset, &op).map_err(invalid)?;
        max_height = max_height.max(u64::from(fv.operand_stack_height()));
    }
    ops.finish().map_err(invalid)?;
    let slots = params.saturating_add(locals).saturating_add(max_height);
    info.frame = FRAME_BASE_BYTES.saturating_add(FRAME_SLOT_BYTES.saturating_mul(slots));
    Ok(info)
}

/// The full deployment-time static check (docs/ABI.md sections 1 and 5). `max_memory_bytes` is `MAX_WASM_MEMORY`.
/// Returns the frame estimates of the defined functions (bytes, in function order) for the runtime stack metering
/// instrumentation.
pub fn validate_code(code: &[u8], max_memory_bytes: u64) -> Result<Vec<u64>, CodeError> {
    let mut validator = Validator::new_with_features(FEATURES);
    let mut m = Module::default();
    let max_pages = max_memory_bytes / WASM_PAGE_BYTES;

    for payload in Parser::new(0).parse_all(code) {
        let payload = payload.map_err(invalid)?;
        let valid = validator.payload(&payload).map_err(invalid)?;
        match payload {
            Payload::TypeSection(reader) => {
                for group in reader {
                    for sub in group.map_err(invalid)?.into_types() {
                        let sig = match &sub.composite_type.inner {
                            CompositeInnerType::Func(f) => Some(Sig {
                                params: f.params().to_vec(),
                                results: f.results().to_vec(),
                            }),
                            _ => None,
                        };
                        m.types.push(sig);
                    }
                }
            }
            Payload::ImportSection(reader) => {
                for imports in reader {
                    match imports.map_err(invalid)? {
                        Imports::Single(_, import) => {
                            check_import(import.module, import.name, import.ty, &mut m)?
                        }
                        Imports::Compact1 { module, items } => {
                            for item in items {
                                let item = item.map_err(invalid)?;
                                check_import(module, item.name, item.ty, &mut m)?;
                            }
                        }
                        Imports::Compact2 { module, ty, names } => {
                            for name in names {
                                check_import(module, name.map_err(invalid)?, ty, &mut m)?;
                            }
                        }
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for ty in reader {
                    m.func_types.push(ty.map_err(invalid)?);
                }
            }
            Payload::TableSection(reader) => {
                for table in reader {
                    let table = table.map_err(invalid)?;
                    m.tables = m.tables.saturating_add(1);
                    if table.ty.element_type != RefType::FUNCREF {
                        return Err(CodeError::Table("element type must be funcref".into()));
                    }
                    let max_ok = table
                        .ty
                        .maximum
                        .is_some_and(|max| max <= MAX_TABLE_ELEMENTS as u64);
                    if table.ty.initial > MAX_TABLE_ELEMENTS as u64 || !max_ok {
                        return Err(CodeError::Table(format!(
                            "must declare a maximum element count not exceeding {MAX_TABLE_ELEMENTS}"
                        )));
                    }
                }
            }
            Payload::MemorySection(reader) => {
                for mem in reader {
                    let mem = mem.map_err(invalid)?;
                    m.memories = m.memories.saturating_add(1);
                    if mem.initial > max_pages {
                        return Err(CodeError::Memory(format!(
                            "initial {} pages exceed the limit of {max_pages} pages",
                            mem.initial
                        )));
                    }
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let e = export.map_err(invalid)?;
                    m.exports.insert(e.name.to_string(), (e.kind, e.index));
                }
            }
            _ => {}
        }
        if let ValidPayload::Func(to_validate, body) = valid {
            let params = m
                .sig_of_func(to_validate.index)
                .map(|s| s.params.len() as u64)
                .ok_or_else(|| CodeError::Invalid("function type does not exist".into()))?;
            let info = analyze_body(to_validate, &body, params)?;
            m.bodies.push(info);
        }
    }

    if m.memories != 1 {
        return Err(CodeError::Memory(format!(
            "exactly one linear memory must be defined, got {}",
            m.memories
        )));
    }
    if m.tables > 1 {
        return Err(CodeError::Table("at most one table".into()));
    }
    check_exports(&m)?;
    Ok(m.bodies.iter().map(|b| b.frame).collect())
}

fn check_exports(m: &Module) -> Result<(), CodeError> {
    match m.exports.get(EXPORT_MEMORY) {
        Some((ExternalKind::Memory, 0)) => {}
        _ => return Err(CodeError::Export(EXPORT_MEMORY.into())),
    }
    let expect = |name: &str, params: &[ValType], results: &[ValType]| match m.exports.get(name) {
        Some((ExternalKind::Func, index)) => match m.sig_of_func(*index) {
            Some(sig) if sig.params == params && sig.results == results => Ok(()),
            _ => Err(CodeError::Export(format!(
                "signature of {name} does not match"
            ))),
        },
        _ => Err(CodeError::Export(name.into())),
    };
    expect(EXPORT_ALLOC, &[I32], &[I32])?;
    expect(EXPORT_CALL, &[I32, I32], &[I64])
}
