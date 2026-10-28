//! Ksana VM: wasmtime engine, deployment-time static checks, host functions and gas table (SPEC §9; the interface is
//! in docs/ABI.md).
//!
//! This crate is consensus code: it must be deterministic, `unwrap` / `panic` are forbidden, errors are always
//! returned. The execution layer provides state access through [`Host`]; the VM is responsible for gas, length limits,
//! the call stack and the ABI rules.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::float_arithmetic,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::iter_over_hash_type
)]
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

mod engine;
pub mod gas;
mod host;
mod instrument;
pub mod limits;
mod validate;

pub use engine::{Vm, INIT_METHOD, MAX_CALL_INPUT_BYTES};
pub use host::{
    AssetMetaInput, AssetPolicy, CallOutcome, CallRequest, ContractCode, FailureReason, Host,
    HostError, NativeOutput, NativeTxInfo, VmError,
};
pub use validate::{CodeError, FEATURES, HOST_FUNCTIONS, HOST_MODULE};
