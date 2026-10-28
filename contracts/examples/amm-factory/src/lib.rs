//! Example contract: an AMM pair factory (D-073: contracts creating contracts).
//!
//! The factory records the code hash of the AMM contract (the AMM code must already be on chain, e.g. an AMM instance
//! has been deployed). `create_pair` creates the pair contract with `create_contract`: the two sides sorted by ID (by
//! kind when the IDs are equal) are the pair's canonical order and `salt = sha256(Borsh(the two sides in canonical
//! order))`, so the same pair can be created only once and its address can be computed in advance
//! (`created_contract_id(factory, salt, AMM code hash)`).
//!
//! Arguments and return values are Borsh-encoded:
//!
//! | Method | Arguments | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `amm_code_hash: [u8; 32]` | `()` |
//! | `create_pair` | `(side_a: Side, side_b: Side)` | `[u8; 32]` (the pair contract ID) |
//! | `get_pair` | `(side_a: Side, side_b: Side)` | `Option<[u8; 32]>` (the sides in either order) |
//! | `pair_count` | none | `u64` |
//! | `pair_at` | `index: u64` | `Option<[u8; 32]>` |
//! | `amm_code_hash` | none | `[u8; 32]` |
//!
//! `Side` is defined as in the AMM example: a 1-byte variant (0 = token contract, 1 = built-in asset) + a 32-byte ID.
//! The pair contract's `init` receives the two sides in canonical order.
//! Event: `("pair_created", side_a, side_b, pair)`.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use ksana_contract_sdk::borsh::BorshSerialize;
use ksana_contract_sdk::{self as sdk, storage, CallResult, Decode};

sdk::entry!(dispatch);

const KEY_CODE_HASH: &[u8] = b"code";
const KEY_COUNT: &[u8] = b"count";
const PREFIX_PAIR: &[u8] = b"p";
const PREFIX_INDEX: &[u8] = b"i";

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_SAME_SIDE: &[u8] = b"side_a and side_b must differ";
const ERR_PAIR_EXISTS: &[u8] = b"pair already exists";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";

/// One side of a pair (encoded like the AMM example's `Side`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize)]
#[borsh(crate = "ksana_contract_sdk::borsh")]
pub enum Side {
    /// A token contract ID.
    Token([u8; 32]),
    /// A built-in asset ID.
    Asset([u8; 32]),
}

impl Side {
    /// The contract ID or asset ID.
    pub fn id(&self) -> [u8; 32] {
        match self {
            Side::Token(id) | Side::Asset(id) => *id,
        }
    }
}

impl Decode for Side {
    fn decode(input: &mut &[u8]) -> Option<Self> {
        match u8::decode(input)? {
            0 => Some(Side::Token(<[u8; 32]>::decode(input)?)),
            1 => Some(Side::Asset(<[u8; 32]>::decode(input)?)),
            _ => None,
        }
    }
}

/// Canonical order: ascending by ID; by kind when the IDs are equal (`Token` first).
pub fn canonical(a: Side, b: Side) -> (Side, Side) {
    if (a.id(), a) <= (b.id(), b) {
        (a, b)
    } else {
        (b, a)
    }
}

/// The pair's salt: `sha256(Borsh(the two sides in canonical order))`.
pub fn pair_salt(a: Side, b: Side) -> [u8; 32] {
    sdk::sha256(&sdk::encode(&canonical(a, b)))
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let code_hash: [u8; 32] = sdk::decode(args)?;
            if storage::get(KEY_CODE_HASH).is_some() {
                return Err(ERR_ALREADY_INITIALIZED.to_vec());
            }
            storage::write(KEY_CODE_HASH, &code_hash);
            storage::write(KEY_COUNT, &0u64);
            sdk::ok(&())
        }
        "create_pair" => {
            let (a, b) = sdk::decode(args)?;
            sdk::ok(&create_pair(a, b)?)
        }
        "get_pair" => {
            let (a, b) = sdk::decode(args)?;
            sdk::ok(&get_pair(a, b)?)
        }
        "pair_count" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&pair_count()?)
        }
        "pair_at" => {
            let index: u64 = sdk::decode(args)?;
            let pair: Option<[u8; 32]> = storage::read(&index_key(index))?;
            sdk::ok(&pair)
        }
        "amm_code_hash" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&code_hash()?)
        }
        _ => sdk::revert("unknown method"),
    }
}

fn code_hash() -> Result<[u8; 32], Vec<u8>> {
    storage::read(KEY_CODE_HASH)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

fn pair_count() -> Result<u64, Vec<u8>> {
    storage::read(KEY_COUNT)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

fn pair_key(salt: &[u8; 32]) -> Vec<u8> {
    [PREFIX_PAIR, salt.as_slice()].concat()
}

fn index_key(index: u64) -> Vec<u8> {
    [PREFIX_INDEX, index.to_le_bytes().as_slice()].concat()
}

/// `get_pair(a, b)`: the sides may be in either order.
pub fn get_pair(a: Side, b: Side) -> Result<Option<[u8; 32]>, Vec<u8>> {
    storage::read(&pair_key(&pair_salt(a, b)))
}

/// `create_pair(a, b) -> pair`.
pub fn create_pair(a: Side, b: Side) -> Result<[u8; 32], Vec<u8>> {
    if a.id() == b.id() {
        return Err(ERR_SAME_SIDE.to_vec());
    }
    let code_hash = code_hash()?;
    let salt = pair_salt(a, b);
    if storage::get(&pair_key(&salt)).is_some() {
        return Err(ERR_PAIR_EXISTS.to_vec());
    }
    let (lo, hi) = canonical(a, b);
    let pair = sdk::create_contract(&code_hash, &salt, &sdk::encode(&(lo, hi)), &[]);
    let index = pair_count()?;
    storage::write(&pair_key(&salt), &pair);
    storage::write(&index_key(index), &pair);
    storage::write(
        KEY_COUNT,
        &index.checked_add(1).ok_or_else(|| ERR_OVERFLOW.to_vec())?,
    );
    sdk::emit(&("pair_created", lo, hi, pair));
    Ok(pair)
}
