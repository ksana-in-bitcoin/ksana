//! Ksana's BTC integration (SPEC §5.1, §7, §12).
//!
//! - [`data`]: construction and parsing of KSN1 data outputs (`OP_RETURN <magic> <chunk>…`), extraction of the
//!   payload from a transaction (§7.1; OP_RETURN outputs and envelopes in witnesses taken together must be exactly
//!   one, D-081).
//! - [`envelope`]: the Taproot witness envelope for settlement and republish payloads (D-081): construction and parsing
//!   of the leaf script, the commitment output and the control block.
//! - [`native`]: recognition of native transactions (path C) and the forced-inclusion window (§7.4).
//! - [`spend`]: signature checks of the producer's inputs in settlement transactions (P2TR key-path, P2WPKH) and the
//!   allowed change scripts (D-034).
//! - [`settlement`]: assembly, signing and checking of settlement transactions (commit and reveal, D-081) (§7.2);
//!   the split transaction for funding UTXOs (D-075).
//! - [`payload`]: the correspondence between blocks and settlement payloads: which blocks' bodies must land on BTC,
//!   payload encoding and length, carried bodies (shared by node and indexer).
//! - [`client`]: the bitcoind access interface [`client::BtcClient`] and its implementation based on `corepc-client`.
//! - [`tracker`]: the BTC tracker, snapshots, the settled prefix and the execution layer's BTC view (shared by node and
//!   indexer, D-049).
//! - [`headers`]: the block DAG (GHOSTDAG) and main-chain selection built only from the headers in payloads on BTC
//!   (shared by node and indexer, D-063).
//! - [`finality`]: BTC-anchored finality (the checkpoint rule, D-082): deciding the finalized chain block by block per
//!   BTC height, shared by node and indexer.
//! - [`bodies`]: determining the bodies of fast-lane blocks from BTC payloads (candidate bodies, derivation and
//!   conversion of forced-inclusion entries, D-077): the indexer rebuilds all bodies from this, the node rebuilds the
//!   blocks the finality decision is waiting for (D-093), shared by node and indexer.
//! - [`prune`]: the pruning policy and the checkpoint files of node and indexer (D-097; local policy and local
//!   formats, not consensus).
//! - [`verdict`]: the verdict on a block the finality decision asks about: executing it as a main-chain block on top of
//!   its selected parent (D-101), shared by node and indexer.
//! - `mock` (feature `mock`): an in-memory mock BTC chain for the node's unit tests.
//!
//! Path A (PSBT, contributions and the user's input/output pairs) has been deleted (D-078).
//!
//! `data`, `envelope`, `native`, `spend`, `settlement`, `headers`, `finality`, `bodies` and `verdict` are consensus
//! code: they must be deterministic, `unwrap` / `panic` are forbidden, errors are always returned.
//! All BTC transaction IDs and block hashes are stored in Bitcoin's internal byte order (D-001).

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

pub mod bodies;
pub mod client;
pub mod data;
pub mod envelope;
pub mod finality;
pub mod headers;
#[cfg(feature = "mock")]
pub mod mock;
pub mod native;
pub mod payload;
pub mod prune;
pub mod settlement;
pub mod spend;
pub mod tracker;
pub mod verdict;
