//! The VM's length and resource limits (consensus parameters, see docs/ABI.md).
//!
//! `MAX_WASM_MEMORY` and `MAX_CALL_DEPTH` are defined per network in `ksana_primitives::params`.

/// wasm page size.
pub const WASM_PAGE_BYTES: u64 = 65_536;
/// Maximum length of a contract storage key.
pub const MAX_STORAGE_KEY_BYTES: usize = 256;
/// Maximum length of a contract storage value.
pub const MAX_STORAGE_VALUE_BYTES: usize = 65_536;
/// Maximum length of a single event.
pub const MAX_EVENT_BYTES: usize = 16_384;
/// Maximum length of the meta of `asset_issue` and `asset_create`.
pub const MAX_ASSET_META_BYTES: usize = 1_024;
/// Maximum encoded length of the funds attached to a contract-to-contract call (Borsh `Vec<Funds>`): `MAX_CALL_FUNDS`
/// items of 48 bytes each, plus a 4-byte length.
pub const MAX_FUNDS_BYTES: usize = 4 + 48 * ksana_primitives::MAX_CALL_FUNDS;
/// Maximum length of a contract's return value (Borsh `Result<Vec<u8>, Vec<u8>>`).
pub const MAX_OUTPUT_BYTES: usize = 65_536;
/// Maximum number of table elements.
pub const MAX_TABLE_ELEMENTS: usize = 4_096;
/// Upper bound on the estimated stack usage of the current call chain per contract instance (bytes, D-014; metered at
/// runtime since D-073, exceeding it traps).
pub const STACK_BUDGET: u64 = 262_144;
/// Single function frame estimate: fixed part (bytes).
pub const FRAME_BASE_BYTES: u64 = 64;
/// Single function frame estimate: bytes per value slot (parameters, locals, operand stack).
pub const FRAME_SLOT_BYTES: u64 = 16;
/// Maximum number of contract instances created within one op (every call creates a new instance).
pub const MAX_INSTANCES_PER_OP: usize = 64;
/// Upper bound on the total linear memory of all instances within one op (bytes).
pub const MAX_TOTAL_MEMORY_PER_OP: u64 = 256 * 1024 * 1024;
/// wasmtime's `max_wasm_stack` (bytes). The static stack check guarantees it is never reached, see D-014.
pub const MAX_WASM_STACK: usize = 32 * 1024 * 1024;
/// Stack size of the dedicated block execution thread (bytes), must be greater than `MAX_WASM_STACK`.
pub const EXECUTION_THREAD_STACK: usize = 64 * 1024 * 1024;
