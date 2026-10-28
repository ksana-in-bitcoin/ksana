//! Independent reference implementation: computes the JMT root directly from the key-value set, bypassing both the
//! jmt crate and the storage.
//!
//! Logically the JMT is a compressed 256-bit sparse Merkle tree (branching on the bits of the key hash from high to
//! low):
//! - empty subtree = `SPARSE_MERKLE_PLACEHOLDER_HASH__`;
//! - a subtree containing a single leaf = that leaf = `sha256("JMT::LeafNode" ‖ key_hash ‖ sha256(value))`;
//! - everything else = `sha256("JMT::IntrnalNode" ‖ left ‖ right)`.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

pub(crate) const PLACEHOLDER: [u8; 32] = *b"SPARSE_MERKLE_PLACEHOLDER_HASH__";

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn bit(key_hash: &[u8; 32], index: usize) -> bool {
    (key_hash[index / 8] >> (7 - index % 8)) & 1 == 1
}

/// `leaves` is sorted by key hash in ascending order.
fn subtree(leaves: &[([u8; 32], [u8; 32])], depth: usize) -> [u8; 32] {
    match leaves {
        [] => PLACEHOLDER,
        [(_, leaf)] => *leaf,
        _ => {
            let split = leaves.partition_point(|(key_hash, _)| !bit(key_hash, depth));
            let (left, right) = leaves.split_at(split);
            sha256(&[
                b"JMT::IntrnalNode",
                &subtree(left, depth + 1),
                &subtree(right, depth + 1),
            ])
        }
    }
}

/// The JMT root of a key-value set.
pub(crate) fn root(state: &BTreeMap<Vec<u8>, Vec<u8>>) -> [u8; 32] {
    let mut leaves: Vec<([u8; 32], [u8; 32])> = state
        .iter()
        .map(|(key, value)| {
            let key_hash = sha256(&[key]);
            let leaf = sha256(&[b"JMT::LeafNode", &key_hash, &sha256(&[value])]);
            (key_hash, leaf)
        })
        .collect();
    leaves.sort();
    subtree(&leaves, 0)
}
