//! Ksana contract development kit (SPEC §9.5; the interface is defined in `docs/ABI.md`).
//!
//! Contracts compile to `wasm32-unknown-unknown`. This crate provides:
//!
//! - Safe wrappers of the host functions: [`storage`], [`caller`] / [`self_id`], block height and timestamp,
//!   [`chain_id`], [`gas_left`], BTC height and [`btc_header`], asset burning [`asset_burn`], contracts creating
//!   contracts [`create_contract`], built-in assets ([`asset_balance`] / [`asset_transfer`] / [`asset_issue`], and
//!   the issuer capabilities of D-121: [`asset_create`] / [`asset_freeze`] / [`asset_allow`] / [`asset_pause`] /
//!   [`asset_renounce`] / [`asset_policy`] / [`asset_status`]), funds attached to the call ([`call_funds`] / [`received`], D-073), inter-contract calls ([`call`] / [`call_with_funds`],
//!   the catchable [`try_call`] / [`try_call_with`], delegate call [`delegate_call`]), the BTC transaction carrying a
//!   path-C op [`native_tx`] (D-078), events [`emit_event`], [`sha256`] / [`sha256d`] / [`verify_schnorr`];
//! - The entry macro [`entry!`]: generates the exported functions `ksana_alloc` and `ksana_call`, dispatches the
//!   Borsh-encoded `(method, args)` to the contract's function, and Borsh-encodes the returned
//!   `Result<Vec<u8>, Vec<u8>>` as the output (ABI §2);
//! - Encoding/decoding: [`Decode`] / [`decode`] (decoding fully consistent with Borsh, see below) and [`encode`]
//!   (Borsh);
//! - [`btc`]: 80-byte block headers, transaction IDs, transaction output parsing, BTC Merkle branch verification;
//! - With the `standards` feature, the token standards (`docs/STANDARDS.md`, D-120): `ks20` (fungible tokens) and
//!   `ks721` (non-fungible tokens), each a client for calling such contracts, the events, and the reference
//!   implementation a contract is built on; `standard`, what both share (the declaration, the revert reasons, the
//!   capabilities, `standard::AssetRef` for an asset of either kind); on native targets `conformance`, which checks a
//!   contract against a standard on the mock host. The feature is off by default, so that a contract not built on
//!   them compiles to the same module as before they were added: a contract that uses them depends on this crate
//!   with `features = ["standards"]`;
//! - The global bump allocator and panic handler on wasm32 (a panic traps directly, without any formatting).
//!
//! # How to write a contract
//!
//! ```ignore
//! #![cfg_attr(target_arch = "wasm32", no_std)]
//! extern crate alloc;
//!
//! use ksana_contract_sdk::{self as sdk, CallResult};
//!
//! sdk::entry!(dispatch);
//!
//! pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
//!     match method {
//!         "init" => sdk::ok(&()),
//!         "echo" => {
//!             let v: u128 = sdk::decode(args)?;
//!             sdk::ok(&v)
//!         }
//!         _ => sdk::revert("unknown method"),
//!     }
//! }
//! ```
//!
//! A contract crate must be `no_std` on wasm32 (otherwise std's own panic handler and allocator get linked in), with
//! crate type `cdylib` (plus `rlib` for native unit tests).
//!
//! Release builds are recommended to compile as `cdylib` only: `cargo rustc --release --target wasm32-unknown-unknown
//! -p <contract> --crate-type cdylib` (which is what the build.rs of `ksana-test-contracts` does). When the same build
//! also produces an `rlib`, LTO cannot internalize the Rust symbols and the module becomes noticeably larger (D-021).
//!
//! # Stack depth: runtime metering
//!
//! Recursion, function pointers, trait objects and `core::fmt` (`format!`, `to_string()`, `panic!` with arguments) may
//! all be used (D-073, replacing the restrictions of D-014 / D-021): before compiling, the VM inserts stack-depth
//! metering into every function, and it traps deterministically when the estimated stack usage of a contract instance's
//! current call chain exceeds `STACK_BUDGET` (256 KiB) (ABI §5). For example, a recursion with about 100 bytes per
//! stack frame can go a little over 2 000 levels deep. Decoding arguments with this crate's [`decode`] / [`Decode`] is
//! still recommended: same wire format as Borsh, no error message formatting on failure, smaller code.
//!
//! # Native tests
//!
//! On non-wasm32 targets the host functions are implemented by an in-memory mock (the [`mock`] module, thread-local
//! state) whose semantics match `docs/ABI.md`: multiple contracts can be deployed / called (inter-contract calls are
//! dispatched to registered Rust functions), blocks and BTC block headers can be set, balances and events can be
//! queried.

#![cfg_attr(target_arch = "wasm32", no_std)]
#![deny(
    missing_docs,
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

extern crate alloc;

mod api;
pub mod btc;
mod codec;
#[cfg(all(any(feature = "standards", test), not(target_arch = "wasm32")))]
pub mod conformance;
#[doc(hidden)]
pub mod entry;
#[cfg(any(feature = "standards", test))]
pub mod ks20;
#[cfg(any(feature = "standards", test))]
pub mod ks721;
mod read;
#[cfg(any(feature = "standards", test))]
pub mod standard;
pub mod storage;

#[cfg(not(target_arch = "wasm32"))]
pub mod mock;
#[cfg(target_arch = "wasm32")]
mod wasm;

// Host function backend: on wasm32 the imports of the `ksana` module, on other targets the in-memory mock. Both provide
// functions with the same names and signatures.
#[cfg(not(target_arch = "wasm32"))]
use mock::host as backend;
#[cfg(target_arch = "wasm32")]
use wasm::host as backend;

pub use api::{
    asset_allow, asset_balance, asset_burn, asset_create, asset_freeze, asset_id, asset_issue,
    asset_pause, asset_policy, asset_renounce, asset_status, asset_transfer, block_height,
    block_timestamp, btc_anchor_height, btc_header, call, call_funds, call_with_funds, caller,
    chain_id, contract_id, create_contract, created_contract_id, delegate_call, emit, emit_event,
    gas_left, native_tx, received, self_id, sha256, sha256d, trap, try_call, try_call_with,
    verify_schnorr, AssetMetaInput, AssetPolicy, CallError, Funds, NativeOutput, NativeTx,
    CAP_ALLOWLIST, CAP_FREEZE, CAP_MINT, CAP_PAUSE, KSA_ASSET_ID, MAX_CALL_FUNDS, STATUS_ALLOWED,
    STATUS_FROZEN,
};
pub use codec::{decode, encode, ok, revert, CallResult, Decode, ERR_MALFORMED_ARGS};

/// Re-export of `borsh`; contracts can derive serialization with `ksana_contract_sdk::borsh::BorshSerialize`
/// (add `#[borsh(crate = "ksana_contract_sdk::borsh")]` when deriving). Use [`Decode`] for deserialization.
pub use borsh;
