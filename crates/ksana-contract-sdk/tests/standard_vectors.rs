//! The shared test vectors of the token standards, `tests/vectors/standards.json` (D-120), are what this crate encodes:
//! the committed file equals what the generator (`examples/gen_standard_vectors`) builds from the `standards` modules.
//! The TS SDK checks the same file (`sdk/ts/test/standards.test.ts`).

#[path = "../examples/gen_standard_vectors/vectors.rs"]
mod vectors;

#[test]
fn the_committed_vectors_are_what_the_sdk_encodes() {
    let path = format!(
        "{}/../../tests/vectors/standards.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let committed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        committed == vectors::vectors(),
        "{path} differs from what the SDK encodes: regenerate it with \
         `cargo run -p ksana-contract-sdk --example gen_standard_vectors` if the change is intended"
    );
}
