//! Unit tests and property tests (they need access to the internal storage, so they live inside the crate).
//!
//! - `oracle`: a reference implementation that computes the root directly from the key-value set, bypassing jmt and
//!   the storage;
//! - `consistency`: reads the column families directly and checks the storage invariants (no garbage nodes, no
//!   missing nodes, correct stale index and value history);
//! - `model`: random operation sequences compared against an in-memory model;
//! - `unit`: specific behaviors and jmt edge cases.

mod consistency;
mod model;
mod oracle;
mod unit;
