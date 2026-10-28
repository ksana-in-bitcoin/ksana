//! The library of the fuzz crate: the differential test of the TypeScript implementations against the Rust ones
//! ([`differential`], D-133), shared by the libFuzzer target `ts_differential` and the stable sweep
//! `examples/differential.rs`.

pub mod differential;
