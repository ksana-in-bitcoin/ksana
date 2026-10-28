//! Generates the shared test vectors of the token standards, `tests/vectors/standards.json` (D-120): the arguments, the
//! answers and the events of the standards' methods, KS-721 token IDs and `AssetRef`, as this crate encodes them. The
//! TS SDK checks its encoders and decoders against the file (`sdk/ts/test/standards.test.ts`), and
//! `tests/standard_vectors.rs` fails when the file is no longer what this crate encodes.
//!
//! Usage: `cargo run -p ksana-contract-sdk --example gen_standard_vectors [output file]`; the default is
//! `tests/vectors/standards.json` at the repository root. Regenerate it only when an encoding of the standards changes
//! on purpose, and record the change in DECISIONS.md.

use std::path::PathBuf;

mod vectors;

fn main() {
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors/standards.json")
        });
    let mut text = serde_json::to_string_pretty(&vectors::vectors()).unwrap();
    text.push('\n');
    std::fs::write(&out, text).unwrap();
    println!("wrote {}", out.display());
}
