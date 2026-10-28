//! Merkle tool tests (RFC 6962, DECISIONS D-004).

use ksana_crypto::{entries_root, merkle_root};
use ksana_primitives::codec::encode;
use ksana_primitives::strategies;
use proptest::prelude::*;

/// RFC 6962 reference test data (the leaves used by Certificate Transparency's merkle_tree_test);
/// the expected roots were computed by an independent Python implementation and match the values published by the CT
/// reference implementation.
#[test]
fn rfc6962_reference_roots() {
    let leaves: Vec<Vec<u8>> = [
        "",
        "00",
        "10",
        "2021",
        "3031",
        "40414243",
        "5051525354555657",
        "606162636465666768696a6b6c6d6e6f",
    ]
    .iter()
    .map(|h| hex::decode(h).unwrap())
    .collect();
    let expected = [
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
        "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125",
        "aeb6bcfe274b70a14fb067a5e5578264db0fa9b51af5e0ba159158f329e06e77",
        "d37ee418976dd95753c1c73862b9398fa2a2cf9b4ff0fdfe8b30cd95209614b7",
        "4e3bbb1f7b478dcfe71fb631631519a3bca12c9aefca1612bfce4c13a86264d4",
        "76e67dadbcdf1e10e1b74ddc608abd2f98dfb16fbce75277b5232a127f2087ef",
        "ddb89be403809e325750d3d263cd78929c2942b7942a34b77e122c9594a74c8c",
        "5dc9da79a70659a9ad559cb701ded9a2ab9d823aad2f4960cfe370eff4604328",
    ];
    for (n, want) in expected.iter().enumerate() {
        assert_eq!(hex::encode(merkle_root(&leaves[..n])), *want, "{n} leaves");
    }
}

proptest! {
    /// The leaves of entries_root are the Borsh encodings of the entries.
    #[test]
    fn entries_root_uses_borsh_leaves(entries in proptest::collection::vec(strategies::entry(), 0..6)) {
        let leaves: Vec<Vec<u8>> = entries.iter().map(|e| encode(e).unwrap()).collect();
        prop_assert_eq!(entries_root(&entries).unwrap(), merkle_root(&leaves));
    }

    /// Any change to the leaf data or order changes the root (domain separation keeps single-leaf trees and internal
    /// nodes from being confused).
    #[test]
    fn root_commits_to_leaves_and_order(
        leaves in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..8), 1..9),
        which in any::<prop::sample::Index>(),
    ) {
        let root = merkle_root(&leaves);
        let i = which.index(leaves.len());

        let mut changed = leaves.clone();
        changed[i].push(0xAA);
        prop_assert_ne!(merkle_root(&changed), root);

        let mut appended = leaves.clone();
        appended.push(Vec::new());
        prop_assert_ne!(merkle_root(&appended), root);

        if leaves.len() >= 2 {
            let j = (i + 1) % leaves.len();
            if leaves[i] != leaves[j] {
                let mut swapped = leaves.clone();
                swapped.swap(i, j);
                prop_assert_ne!(merkle_root(&swapped), root);
            }
        }
    }
}

/// No Bitcoin-style duplication of odd nodes: [a, b, c] and [a, b, c, c] have different roots (CVE-2012-2459).
#[test]
fn no_duplicate_leaf_malleability() {
    let three = [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()];
    let four = [b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"c".to_vec()];
    assert_ne!(merkle_root(&three), merkle_root(&four));
}
