//! Deterministic runtime stack metering (D-073, replacing D-014's deployment-time requirement of an "acyclic call
//! graph").
//!
//! Before compilation, a mutable i32 global (indexed after the existing globals) is added to the module to record the
//! estimated stack usage of this instance's current call chain, and every defined function is rewritten:
//!
//! ```text
//! global.get $sh  i32.const F  i32.add  global.set $sh        ;; F = the function's frame estimate (ABI section 5)
//! global.get $sh  i32.const STACK_BUDGET  i32.gt_u
//! if  unreachable  end                                          ;; over budget: trap, a deterministic op failure
//! block (result type same as the function)
//!   original function body (`return` becomes a `br` out of this block)
//! end
//! global.get $sh  i32.const F  i32.sub  global.set $sh
//! ```
//!
//! The metering depends only on the bytecode and is architecture-independent. After a trap the instance is no longer
//! used (every call creates a new instance), so the counter need not be restored. What is stored is still the original
//! bytecode (`code_hash` unchanged); the instrumentation is a consensus rule applied before compilation, and the
//! inserted instructions consume gas as well.

use std::convert::Infallible;

use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, GlobalSection, GlobalType, Instruction, SectionId,
    TypeSection,
};
use wasmparser::{CompositeInnerType, Imports, Operator, Parser, Payload, TypeRef, ValType};

use crate::validate::{CodeError, STACK_BUDGET};

type RError = reencode::Error<Infallible>;

fn invalid(e: impl std::fmt::Display) -> CodeError {
    CodeError::Invalid(e.to_string())
}

/// The module layout needed for instrumentation.
#[derive(Default)]
struct Layout {
    /// The result types of each type (`None` for non-function types).
    results: Vec<Option<Vec<ValType>>>,
    /// The type index of each defined function.
    defined: Vec<u32>,
    /// The total number of globals (imported + defined): the index of the added counter variable.
    globals: u32,
}

impl Layout {
    fn scan(code: &[u8]) -> Result<Self, CodeError> {
        let mut layout = Layout::default();
        for payload in Parser::new(0).parse_all(code) {
            match payload.map_err(invalid)? {
                Payload::TypeSection(reader) => {
                    for group in reader {
                        for sub in group.map_err(invalid)?.into_types() {
                            layout.results.push(match &sub.composite_type.inner {
                                CompositeInnerType::Func(f) => Some(f.results().to_vec()),
                                _ => None,
                            });
                        }
                    }
                }
                Payload::ImportSection(reader) => {
                    for imports in reader {
                        let globals = match imports.map_err(invalid)? {
                            Imports::Single(_, import) => {
                                u32::from(matches!(import.ty, TypeRef::Global(_)))
                            }
                            Imports::Compact1 { items, .. } => {
                                let mut n = 0u32;
                                for item in items {
                                    if matches!(item.map_err(invalid)?.ty, TypeRef::Global(_)) {
                                        n = n.saturating_add(1);
                                    }
                                }
                                n
                            }
                            Imports::Compact2 { ty, names, .. } => {
                                if matches!(ty, TypeRef::Global(_)) {
                                    names.count()
                                } else {
                                    0
                                }
                            }
                        };
                        layout.globals = layout.globals.saturating_add(globals);
                    }
                }
                Payload::FunctionSection(reader) => {
                    for ty in reader {
                        layout.defined.push(ty.map_err(invalid)?);
                    }
                }
                Payload::GlobalSection(reader) => {
                    layout.globals = layout.globals.saturating_add(reader.count());
                }
                _ => {}
            }
        }
        Ok(layout)
    }
}

struct Instrumenter<'a> {
    /// The frame estimate of each defined function (bytes).
    frames: &'a [u64],
    /// The wrapper block type of each defined function.
    blocks: Vec<BlockType>,
    /// The types added for multi-value results (appended at the end of the type section).
    extra_types: Vec<Vec<wasm_encoder::ValType>>,
    /// The index of the counter variable.
    counter: u32,
    counter_added: bool,
    next_body: usize,
    error: Option<CodeError>,
}

fn encoder_type(ty: ValType) -> Result<wasm_encoder::ValType, CodeError> {
    wasm_encoder::ValType::try_from(ty)
        .map_err(|e| CodeError::Invalid(format!("value type: {e:?}")))
}

impl<'a> Instrumenter<'a> {
    fn new(layout: &Layout, frames: &'a [u64]) -> Result<Self, CodeError> {
        if layout.defined.len() != frames.len() {
            return Err(CodeError::Invalid(
                "number of functions does not match the analysis result".into(),
            ));
        }
        let original_types = u32::try_from(layout.results.len()).map_err(invalid)?;
        let mut extra_types: Vec<Vec<wasm_encoder::ValType>> = Vec::new();
        let mut blocks = Vec::with_capacity(layout.defined.len());
        for type_index in &layout.defined {
            let results = layout
                .results
                .get(usize::try_from(*type_index).map_err(invalid)?)
                .and_then(Option::as_ref)
                .ok_or_else(|| CodeError::Invalid("function type does not exist".into()))?;
            let block = match results.as_slice() {
                [] => BlockType::Empty,
                [one] => BlockType::Result(encoder_type(*one)?),
                many => {
                    let wanted = many
                        .iter()
                        .map(|t| encoder_type(*t))
                        .collect::<Result<Vec<_>, _>>()?;
                    let position = match extra_types.iter().position(|t| *t == wanted) {
                        Some(p) => p,
                        None => {
                            extra_types.push(wanted);
                            extra_types.len().saturating_sub(1)
                        }
                    };
                    let index = original_types
                        .checked_add(u32::try_from(position).map_err(invalid)?)
                        .ok_or_else(|| CodeError::Invalid("type count overflow".into()))?;
                    BlockType::FunctionType(index)
                }
            };
            blocks.push(block);
        }
        Ok(Instrumenter {
            frames,
            blocks,
            extra_types,
            counter: layout.globals,
            counter_added: false,
            next_body: 0,
            error: None,
        })
    }

    fn add_counter(&mut self, globals: &mut GlobalSection) {
        globals.global(
            GlobalType {
                val_type: wasm_encoder::ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        self.counter_added = true;
    }

    /// Rewrites one function body; on error, records the error and returns `None` (the error type of `Reencode` does
    /// not carry custom errors).
    fn rewrite(
        &mut self,
        func: &wasmparser::FunctionBody<'_>,
    ) -> Result<Option<wasm_encoder::Function>, RError> {
        let index = self.next_body;
        self.next_body = self.next_body.saturating_add(1);
        let (Some(frame), Some(block)) = (self.frames.get(index), self.blocks.get(index)) else {
            self.error = Some(CodeError::Invalid(
                "more function bodies than function declarations".into(),
            ));
            return Ok(None);
        };
        let (Ok(frame), Ok(budget)) = (i32::try_from(*frame), i32::try_from(STACK_BUDGET)) else {
            self.error = Some(CodeError::Invalid("frame estimate exceeds i32".into()));
            return Ok(None);
        };
        let block = *block;
        let counter = self.counter;
        let mut f = self.new_function_with_parsed_locals(func)?;
        for ins in [
            Instruction::GlobalGet(counter),
            Instruction::I32Const(frame),
            Instruction::I32Add,
            Instruction::GlobalSet(counter),
            Instruction::GlobalGet(counter),
            Instruction::I32Const(budget),
            Instruction::I32GtU,
            Instruction::If(BlockType::Empty),
            Instruction::Unreachable,
            Instruction::End,
            Instruction::Block(block),
        ] {
            f.instruction(&ins);
        }
        // the nesting depth within the original function body (excluding the body itself): `return` becomes
        // `br depth`, jumping to the exit after the wrapper block
        let mut depth: u32 = 0;
        let mut reader = func.get_operators_reader()?;
        while !reader.eof() {
            let op = reader.read()?;
            match op {
                Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                    depth = depth.saturating_add(1);
                    f.instruction(&self.instruction(op)?);
                }
                Operator::End if depth == 0 => {
                    // the end of the function body: close the wrapper block, subtract this frame at the exit, then end
                    // the function
                    for ins in [
                        Instruction::End,
                        Instruction::GlobalGet(counter),
                        Instruction::I32Const(frame),
                        Instruction::I32Sub,
                        Instruction::GlobalSet(counter),
                        Instruction::End,
                    ] {
                        f.instruction(&ins);
                    }
                }
                Operator::End => {
                    depth = depth.saturating_sub(1);
                    f.instruction(&Instruction::End);
                }
                Operator::Return => {
                    f.instruction(&Instruction::Br(depth));
                }
                other => {
                    f.instruction(&self.instruction(other)?);
                }
            }
        }
        reader.finish()?;
        Ok(Some(f))
    }
}

impl Reencode for Instrumenter<'_> {
    type Error = Infallible;

    fn parse_type_section(
        &mut self,
        types: &mut TypeSection,
        section: wasmparser::TypeSectionReader<'_>,
    ) -> Result<(), RError> {
        reencode::utils::parse_type_section(self, types, section)?;
        for results in &self.extra_types {
            types.ty().function([], results.iter().copied());
        }
        Ok(())
    }

    fn parse_global_section(
        &mut self,
        globals: &mut GlobalSection,
        section: wasmparser::GlobalSectionReader<'_>,
    ) -> Result<(), RError> {
        reencode::utils::parse_global_section(self, globals, section)?;
        self.add_counter(globals);
        Ok(())
    }

    fn intersperse_section_hook(
        &mut self,
        module: &mut wasm_encoder::Module,
        _after: Option<SectionId>,
        before: Option<SectionId>,
    ) -> Result<(), RError> {
        // the module has no global section: add one where the global section belongs (before the export section and
        // the sections after it)
        let past_globals = matches!(
            before,
            None | Some(
                SectionId::Export
                    | SectionId::Start
                    | SectionId::Element
                    | SectionId::DataCount
                    | SectionId::Code
                    | SectionId::Data
            )
        );
        if !self.counter_added && past_globals {
            let mut globals = GlobalSection::new();
            self.add_counter(&mut globals);
            module.section(&globals);
        }
        Ok(())
    }

    fn parse_function_body(
        &mut self,
        code: &mut CodeSection,
        func: wasmparser::FunctionBody<'_>,
    ) -> Result<(), RError> {
        if let Some(f) = self.rewrite(&func)? {
            code.function(&f);
        }
        Ok(())
    }
}

/// Inserts stack metering into a module that has passed the deployment check; `frames` are the frame estimates of the
/// defined functions (consistent with the analysis result of [`crate::validate`]).
pub(crate) fn instrument(code: &[u8], frames: &[u64]) -> Result<Vec<u8>, CodeError> {
    let layout = Layout::scan(code)?;
    let mut inst = Instrumenter::new(&layout, frames)?;
    let mut module = wasm_encoder::Module::new();
    inst.parse_core_module(&mut module, Parser::new(0), code)
        .map_err(invalid)?;
    if let Some(e) = inst.error {
        return Err(e);
    }
    if inst.next_body != frames.len() {
        return Err(CodeError::Invalid(
            "fewer function bodies than function declarations".into(),
        ));
    }
    if !inst.counter_added {
        return Err(CodeError::Invalid(
            "cannot insert the stack metering variable".into(),
        ));
    }
    Ok(module.finish())
}
