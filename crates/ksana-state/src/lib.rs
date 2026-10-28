//! Ksana state store: Jellyfish Merkle Tree (`jmt` 0.12, SHA-256) + RocksDB,
//! versioned reads/writes, rollback and pruning (SPEC §10).
//!
//! This crate is consensus code: `state_root` is computed here and must be deterministic; `unwrap` / `panic` are
//! forbidden, errors are always returned.
//! Internally only `BTreeMap` / `BTreeSet` are used; the write set is processed in order sorted by full key.
//!
//! # Semantics
//!
//! - **Version** ([`Version`]) is the fast-lane block height. Each [`StateDb::commit`] produces a new version, and
//!   `state_root` is the root hash of that version's JMT ([`StateDb::root`]). Versions must be consecutive: `commit`
//!   only accepts `latest + 1`; the first version of an empty database (genesis) can be any value (except `u64::MAX`,
//!   which jmt reserves as `PRE_GENESIS_VERSION`).
//! - **Keys**: the JMT key hash is `sha256(full state key)` ([`key_hash`]). The full state key already carries the
//!   prefix byte of SPEC §10.1, so it is the spec's `sha256(prefix ‖ key)`. This crate does not interpret keys or
//!   values (encoding in `docs/ABI.md` §7); a value may be an empty byte string, and an empty value is different from
//!   "absent".
//! - **The root is determined only by the final key-value set**, independent of write order, commit boundaries and
//!   history:
//!   - leaf = `sha256("JMT::LeafNode" ‖ key_hash ‖ sha256(value))`;
//!   - internal node = `sha256("JMT::IntrnalNode" ‖ left ‖ right)` (spelled that way in jmt itself);
//!   - empty subtree = the 32-byte constant `b"SPARSE_MERKLE_PLACEHOLDER_HASH__"`, which is also the root of the
//!     empty tree;
//!   - a subtree containing a single leaf is replaced by that leaf (compressed 256-bit sparse Merkle tree).
//! - **No-ops**: deleting a non-existent key and writing a key with its current value do not change the root, and in
//!   this crate they are true no-ops: before committing they are compared against the latest values and filtered
//!   out, producing no tree nodes, value history or change records.
//! - **An empty write set** also produces a new version (just one copied root node) whose root equals the previous
//!   version's; when an empty database starts at genesis with an empty write set, the root is the empty tree's root.
//! - **Rollback**: [`StateDb::rollback_to`] deletes everything after `version`; afterwards different content can be
//!   committed at `version + 1`.
//! - **Pruning**: after [`StateDb::prune`] only versions `>= keep_from` are readable and can be rolled back to;
//!   superseded tree nodes and old values are really deleted. This crate does not prune automatically: the node
//!   should call `prune(latest.saturating_sub(REORG_WINDOW))` after every commit (SPEC §10.2), so that each pruning
//!   handles only one version and the write batch is small.
//! - **Reset**: [`StateDb::reset`] deletes all data and returns to an empty database; the caller uses it to rebuild
//!   from genesis when the old state has been pruned and the fork point cannot be rolled back to (D-093).
//! - Every write operation (commit, rollback, prune, reset) is one RocksDB `WriteBatch`; the version range metadata is
//!   written in the same batch, and the WAL is written with `sync`: a successful return means it is durable, and
//!   after a crash either all of it or none of it takes effect.
//!
//! # Read views
//!
//! [`StateView`] is a read-only view pinned to a version: it holds an `Arc` of the RocksDB handle, the version number
//! and that version's root at creation time; it is `Send + Sync + 'static` and can be put into a wasmtime `Store`.
//! Later commits do not affect what it reads.
//! Every read first checks, within the same RocksDB snapshot, that "the root of that version is still the root at
//! creation time" before reading the value: if the version was later rolled back or pruned (or re-committed with
//! different content after a rollback), the read returns [`StateError::StaleView`] and never silently reads data of
//! another branch. This crate does not prevent rollback or pruning while a view is alive.
//! Views share the RocksDB instance of the `StateDb`; the database closes only after all of them are released.
//!
//! # Storage layout (local format, not part of consensus)
//!
//! | Column family | Key | Value | Purpose |
//! | --- | --- | --- | --- |
//! | `meta` | `"meta"` | format version `u8` ‖ oldest ‖ latest (`u64` big-endian) | available version range |
//! | `jmt_nodes` | version ‖ nibble count `u8` ‖ path (two nibbles per byte) | Borsh(`jmt::storage::Node`) | tree nodes; version first, deleted by range on rollback |
//! | `jmt_stale` | stale_since ‖ node key | empty | jmt's stale node index, used to delete nodes when pruning |
//! | `values` | key_hash ‖ `!version` | `0x00` (deleted) or `0x01` ‖ value | value history; version inverted, one seek yields "the latest value ≤ a given version" |
//! | `latest` | full key | value | key-values of the latest version: `get`, `scan_prefix`, no-op filtering on commit, key preimages |
//! | `changes` | version ‖ full key | empty | keys really changed in each version: delete the value history and restore `latest` on rollback, find superseded old values when pruning |
//! | `roots` | version | 32-byte root | root of each available version; views use it to tell whether they are stale |
//!
//! Version numbers are always encoded as 8 big-endian bytes (inverted in `values`), so "all nodes / stale records /
//! roots after some version" is a contiguous range.
//!
//! Interplay with jmt (behavior of jmt 0.12, confirmed by reading its source, see the comments throughout):
//! - jmt assumes the previous root of version `v` is at `v - 1`. When an empty database starts at genesis `g > 0`,
//!   the reader treats the root at `g - 1` as the empty tree; otherwise a genesis of "empty write set / only deletions"
//!   would write the empty root at version `g - 1`.
//! - jmt does not mark the old root as stale when it copies it on "no change" or when inserting into an empty tree
//!   (Null root); here every commit additionally records "the previous version's root is stale as of this version",
//!   so the stale index covers every node that is no longer reachable and pruning leaves no garbage.
//! - jmt does not change the tree for "deleting a non-existent key" but still records a deletion in
//!   `NodeBatch::values`; for "writing the same value" it rebuilds the whole path from leaf to root in the new version
//!   (root unchanged). Both are avoided by the no-op filtering above.
//! - jmt echoes the written values in `NodeBatch::values`; the value history is written directly from the (filtered)
//!   write set, and the two have the same content.
//! - jmt's `get` does not walk the tree, it only calls `TreeReader::get_value_option`; so historical reads rely on the
//!   value history, which rollback and pruning must maintain exactly (the tests compare `get_at` and the root against
//!   the model version by version).

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

mod error;
pub mod rocks;
mod schema;
mod store;
mod tree;
mod view;

pub use error::StateError;
pub use rocks::DbMemory;
pub use store::StateDb;
pub use view::StateView;

/// Version number, equal to the fast-lane block height.
pub type Version = u64;

/// The write set of one version: full state key → `Some(value)` means write, `None` means delete.
/// `BTreeMap` guarantees a deterministic iteration order.
pub type WriteSet = std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// The JMT key hash of a full state key: `sha256(key)` (SPEC §10.1, `key` already contains the prefix byte).
pub fn key_hash(key: &[u8]) -> [u8; 32] {
    ksana_primitives::hash::sha256(key)
}

/// Read-only state access. Object safe, usable as `&dyn StateRead` / `Box<dyn StateRead>`.
pub trait StateRead: Send + Sync {
    /// The value of `key` at the version this reader corresponds to ([`StateDb`]: the latest version, [`StateView`]:
    /// the view's pinned version).
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StateError>;
}

#[cfg(test)]
mod tests;
