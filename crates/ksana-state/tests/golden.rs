//! Fixed vectors: key hash and state root. The expected values were computed by an independent Python implementation
//! (hashlib; the JMT implemented as a compressed 256-bit sparse Merkle tree: an empty subtree is
//! `SPARSE_MERKLE_PLACEHOLDER_HASH__`, a single-leaf subtree is that leaf
//! `sha256("JMT::LeafNode" ‖ sha256(key) ‖ sha256(value))`, everything else is
//! `sha256("JMT::IntrnalNode" ‖ left ‖ right)`).
//! If a jmt upgrade changes the hashing, this fails -- that is a consensus change.

use ksana_state::{key_hash, StateDb, WriteSet};

fn h(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap()
}

/// A set of key-values shaped like the prefixes of ABI §7 (including an empty value).
fn golden_set() -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        // 0x01 account → 16-byte value (the pre-D-078 AccountState { nonce: 1, fee_debt: 0 }; only the hashing is
        // checked here)
        (
            h("01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            h("01000000000000000000000000000000"),
        ),
        // 0x02 KSA ‖ account → 1000
        (
            h("02\
               0000000000000000000000000000000000000000000000000000000000000000\
               aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            h("e8030000000000000000000000000000"),
        ),
        // 0x05 contract ‖ "k" → "v"
        (
            h("05bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb6b"),
            b"v".to_vec(),
        ),
        // 0x06 code_hash → empty code
        (
            h("06cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
            Vec::new(),
        ),
        // 0x08 prefix, a 33-byte key and a 50-byte value (only the JMT root is checked; the execution layer's key
        // format is in docs/ABI.md section 7)
        (
            h("080000000000000000000000000000000000000000000000000000000000000000"),
            h(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
               3200000000000000000000000000000000",
            ),
        ),
    ]
}

/// Root of the empty tree: the 32 ASCII bytes `SPARSE_MERKLE_PLACEHOLDER_HASH__`.
const EMPTY_ROOT: &str = "5350415253455f4d45524b4c455f504c414345484f4c4445525f484153485f5f";
/// Root of the tree containing only `golden_set()[0]` (= the hash of that leaf).
const SINGLE_ROOT: &str = "11c86f894c3ea49f30e33427c7d1d3bd96468d794b5c594c46091045918090ad";
/// Root of all 5 items of `golden_set()`.
const GOLDEN_ROOT: &str = "e3025377ff01bbbb389d7ec8b74dfbbdd5706957ad702069917661946b5b13b5";

#[test]
fn key_hash_is_sha256_of_full_key() {
    assert_eq!(
        hex::encode(key_hash(&golden_set()[0].0)),
        "b2642d0c3bfa93efe38e99e824962f54343302c923765271c6e99dfcbe43cc91"
    );
    assert_eq!(
        hex::encode(key_hash(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn golden_roots() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = StateDb::open(dir.path()).unwrap();
    assert_eq!(
        hex::encode(state.preview_root(&WriteSet::new()).unwrap()),
        EMPTY_ROOT
    );
    assert_eq!(
        hex::encode(state.commit(0, &WriteSet::new()).unwrap()),
        EMPTY_ROOT
    );

    let set = golden_set();
    let first: WriteSet = [(set[0].0.clone(), Some(set[0].1.clone()))].into();
    assert_eq!(hex::encode(state.commit(1, &first).unwrap()), SINGLE_ROOT);

    let all: WriteSet = set
        .iter()
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .collect();
    assert_eq!(hex::encode(state.preview_root(&all).unwrap()), GOLDEN_ROOT);
    assert_eq!(hex::encode(state.commit(2, &all).unwrap()), GOLDEN_ROOT);
    assert_eq!(hex::encode(state.root(0).unwrap()), EMPTY_ROOT);
    assert_eq!(hex::encode(state.root(1).unwrap()), SINGLE_ROOT);
}

/// The same final set reached through different histories (batches written in reverse order, temporary keys, values
/// changed and changed back, deletions of non-existent keys) yields the same root.
#[test]
fn golden_root_is_history_independent() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = StateDb::open(dir.path()).unwrap();
    let set = golden_set();
    let mut version = 1000;
    let mut commit = |writes: WriteSet| {
        let root = state.commit(version, &writes).unwrap();
        version += 1;
        root
    };
    commit([(b"temp".to_vec(), Some(b"x".to_vec()))].into());
    for (key, value) in set.iter().rev() {
        commit(
            [
                (key.clone(), Some(b"wrong".to_vec())),
                (b"never-existed".to_vec(), None),
            ]
            .into(),
        );
        commit([(key.clone(), Some(value.clone()))].into());
    }
    let root = commit([(b"temp".to_vec(), None)].into());
    assert_eq!(hex::encode(root), GOLDEN_ROOT);
}
