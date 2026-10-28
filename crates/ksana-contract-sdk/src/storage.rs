//! Contract-private key-value storage (ABI §3: `storage_get` / `storage_set` / `storage_remove`).
//!
//! Keys are at most 256 bytes and values at most 65,536 bytes; the host traps when exceeded.

use alloc::vec::Vec;

use borsh::BorshSerialize;

use crate::backend;
use crate::codec::{encode, Decode};

/// Revert reason when a stored value cannot be decoded as the expected type.
pub const ERR_MALFORMED_STORAGE: &[u8] = b"malformed storage value";

/// Reads the raw value of `key`; returns `None` when it does not exist.
pub fn get(key: &[u8]) -> Option<Vec<u8>> {
    backend::storage_get(key)
}

/// Writes the raw value of `key`.
pub fn set(key: &[u8], value: &[u8]) {
    backend::storage_set(key, value);
}

/// Removes `key`.
pub fn remove(key: &[u8]) {
    backend::storage_remove(key);
}

/// Reads and decodes the value of `key`; returns `Ok(None)` when it does not exist and `Err(ERR_MALFORMED_STORAGE)`
/// when it cannot be fully decoded.
pub fn read<T: Decode>(key: &[u8]) -> Result<Option<T>, Vec<u8>> {
    match get(key) {
        None => Ok(None),
        Some(bytes) => crate::decode(&bytes)
            .map(Some)
            .map_err(|_| ERR_MALFORMED_STORAGE.to_vec()),
    }
}

/// Encodes `value` with Borsh and writes it to `key`.
pub fn write<T: BorshSerialize + ?Sized>(key: &[u8], value: &T) {
    set(key, &encode(value));
}
