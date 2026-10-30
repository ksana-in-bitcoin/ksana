//! Merkle tools: RFC 6962 Merkle Tree Hash (SHA-256), used for `entries_root` (DECISIONS D-004).
//!
//! - `MTH({}) = sha256("")`
//! - `MTH({d}) = sha256(0x00 ‖ d)`
//! - `MTH(D[0..n]) = sha256(0x01 ‖ MTH(D[0..k]) ‖ MTH(D[k..n]))`, where `k` is the largest power of 2 less than `n`
//!
//! A root can also be computed from the leaf hashes alone ([`root_of_leaf_hashes`]): the BTC tracker keeps the leaf
//! hash of each native transaction's forced-inclusion entry and computes the roots of the entries a block must
//! force-include from those, without building the entries again (D-135).

use ksana_primitives::codec::encode;
use ksana_primitives::{CodecError, Entry};
use sha2::{Digest, Sha256};

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

/// The leaf hash of the leaf data `data`: `MTH({data}) = sha256(0x00 ‖ data)`.
pub fn leaf_hash(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([LEAF_PREFIX]);
    hasher.update(data);
    hasher.finalize().into()
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([NODE_PREFIX]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// The largest power of 2 less than `n` (requires `n >= 2`).
fn split_point(n: usize) -> usize {
    let mut k = 1usize;
    while let Some(next) = k.checked_mul(2) {
        if next >= n {
            break;
        }
        k = next;
    }
    k
}

/// `MTH(leaves)`, each leaf turned into its leaf hash by `leaf`.
fn mth<T>(leaves: &[T], leaf: &dyn Fn(&T) -> [u8; 32]) -> [u8; 32] {
    match leaves {
        [] => Sha256::digest([]).into(),
        [one] => leaf(one),
        _ => {
            let (left, right) = leaves.split_at(split_point(leaves.len()));
            node_hash(&mth(left, leaf), &mth(right, leaf))
        }
    }
}

/// The Merkle root of a list of leaf data (in the given order).
pub fn merkle_root<T: AsRef<[u8]>>(leaves: &[T]) -> [u8; 32] {
    mth(leaves, &|data: &T| leaf_hash(data.as_ref()))
}

/// The Merkle root of a list of leaves given by their leaf hashes ([`leaf_hash`]), in order: the same root as
/// [`merkle_root`] of the leaf data.
pub fn root_of_leaf_hashes(leaves: &[[u8; 32]]) -> [u8; 32] {
    mth(leaves, &|hash: &[u8; 32]| *hash)
}

/// The leaf hash of an entry in `entries_root`: of its Borsh encoding.
pub fn entry_leaf(entry: &Entry) -> Result<[u8; 32], CodecError> {
    Ok(leaf_hash(&encode(entry)?))
}

/// `entries_root`: the leaves are the Borsh encodings of the block's entries, in entry order (SPEC §6.1). Each entry is
/// encoded and hashed in turn, so that only its leaf hash is kept.
pub fn entries_root(entries: &[Entry]) -> Result<[u8; 32], CodecError> {
    let leaves = entries
        .iter()
        .map(entry_leaf)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(root_of_leaf_hashes(&leaves))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_point_is_largest_power_of_two_below_n() {
        for (n, k) in [(2, 1), (3, 2), (4, 2), (5, 4), (8, 4), (9, 8), (1000, 512)] {
            assert_eq!(split_point(n), k, "n = {n}");
        }
    }
}
