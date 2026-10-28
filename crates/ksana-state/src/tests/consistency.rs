//! Storage consistency check (for tests): reads the column families directly and checks the invariants between them.
//!
//! - `meta` agrees with the in-memory version range; `roots` covers exactly `[oldest, latest]` and equals the roots
//!   recomputed from the root nodes.
//! - The JMT nodes in the database are exactly the nodes reachable from the roots of the kept versions: none missing
//!   (readable), none extra (pruning leaves no garbage); the child hashes recorded in every internal node agree with
//!   the children themselves.
//! - Stale index: the node of every record exists, is reachable at `stale_since - 1` and unreachable at `stale_since`
//!   and after; every node unreachable from the latest root has a stale record (otherwise it could never be pruned).
//! - Value history, change records and the latest index agree with each other; the history `<= oldest` has at most
//!   one entry per key, and it is not a deletion record.

use std::collections::{BTreeMap, BTreeSet};

use jmt::storage::{NibblePath, Node, NodeKey};
use rocksdb::{IteratorMode, DB};
use sha2::Sha256;

use super::oracle::PLACEHOLDER;
use crate::schema::{
    self, decode_stored_value, split_version, Meta, CF_CHANGES, CF_LATEST, CF_META, CF_NODES,
    CF_ROOTS, CF_STALE, CF_VALUES, COLUMN_FAMILIES,
};
use crate::store::{cf_handle, read_value_at};
use crate::{key_hash, tree, StateDb, Version};

/// Value history per key hash: `(version, value or deletion)`, from newest to oldest version.
type History = BTreeMap<[u8; 32], Vec<(Version, Option<Vec<u8>>)>>;

/// All key-values in a column family (sorted by key).
pub(crate) fn entries(db: &DB, cf: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let handle = cf_handle(db, cf).unwrap();
    db.iterator_cf(handle, IteratorMode::Start)
        .map(|item| {
            let (key, value) = item.unwrap();
            (key.to_vec(), value.to_vec())
        })
        .collect()
}

fn node_key(version: Version, nibbles: &[u8]) -> NodeKey {
    NodeKey::new(
        version,
        nibbles.iter().map(|&n| n.into()).collect::<NibblePath>(),
    )
}

fn node_hash(node: &Node) -> [u8; 32] {
    match node {
        Node::Null => PLACEHOLDER,
        Node::Leaf(leaf) => leaf.hash::<Sha256>(),
        Node::Internal(internal) => internal.hash::<Sha256>(),
    }
}

/// All nodes (storage keys) reachable from the root of version `version`. Panics when a node is missing or a child
/// hash does not match.
pub(crate) fn reachable(db: &DB, version: Version) -> BTreeSet<Vec<u8>> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<(Version, Vec<u8>, Option<[u8; 32]>)> = vec![(version, Vec::new(), None)];
    while let Some((node_version, path, expected_hash)) = stack.pop() {
        let key = node_key(node_version, &path);
        let node = tree::read_node(db, &key).unwrap().unwrap_or_else(|| {
            panic!("node {key:?} reachable from version {version} does not exist")
        });
        if let Some(expected) = expected_hash {
            assert_eq!(
                node_hash(&node),
                expected,
                "hash of node {key:?} does not match the parent's record"
            );
        }
        if !seen.insert(schema::node_key(&key).unwrap()) {
            continue;
        }
        match &node {
            Node::Internal(internal) => {
                for (nibble, child) in internal.children_sorted() {
                    let mut child_path = path.clone();
                    child_path.push(u8::from(nibble));
                    stack.push((child.version, child_path, Some(child.hash)));
                }
            }
            Node::Leaf(_) => {}
            Node::Null => assert!(path.is_empty(), "Null node {key:?} at a non-root position"),
        }
    }
    seen
}

/// Checks all invariants, panicking when one does not hold.
pub(crate) fn check(state: &StateDb) {
    let db = state.raw();
    let (oldest, latest) = match (state.oldest_version(), state.latest_version()) {
        (Some(oldest), Some(latest)) => (oldest, latest),
        (None, None) => {
            for cf in COLUMN_FAMILIES {
                assert!(
                    entries(db, cf).is_empty(),
                    "column family {cf} of an empty database contains data"
                );
            }
            return;
        }
        other => panic!("inconsistent in-memory version range: {other:?}"),
    };

    // Metadata and roots.
    let meta = entries(db, CF_META);
    assert_eq!(meta.len(), 1);
    assert_eq!(Meta::decode(&meta[0].1).unwrap(), Meta { oldest, latest });
    let roots = entries(db, CF_ROOTS);
    let root_versions: Vec<Version> = roots
        .iter()
        .map(|(key, _)| {
            let (version, rest) = split_version(key).unwrap();
            assert!(rest.is_empty());
            version
        })
        .collect();
    assert_eq!(root_versions, (oldest..=latest).collect::<Vec<_>>());
    for (version, (_, root)) in root_versions.iter().zip(&roots) {
        assert_eq!(
            root.as_slice(),
            tree::root_from_nodes(db, *version).unwrap().as_slice(),
            "roots column family does not match the root node of version {version}"
        );
    }

    // Nodes: exactly those reachable from the kept versions.
    let mut reachable_at = BTreeMap::new();
    let mut all_reachable = BTreeSet::new();
    for version in oldest..=latest {
        let nodes = reachable(db, version);
        all_reachable.extend(nodes.iter().cloned());
        reachable_at.insert(version, nodes);
    }
    let stored: BTreeSet<Vec<u8>> = entries(db, CF_NODES)
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    let garbage: Vec<_> = stored.difference(&all_reachable).collect();
    assert!(
        garbage.is_empty(),
        "unreachable nodes that were not deleted: {garbage:?}"
    );

    // Stale index.
    let mut stale = BTreeSet::new();
    for (entry, value) in entries(db, CF_STALE) {
        assert!(value.is_empty());
        let (since, node) = split_version(&entry).unwrap();
        assert!(
            since > oldest && since <= latest,
            "stale record {since} is not within ({oldest}, {latest}]"
        );
        assert!(
            stored.contains(node),
            "stale record points at a non-existent node"
        );
        assert!(
            reachable_at[&(since - 1)].contains(node),
            "node stale since {since} is unreachable at {}",
            since - 1
        );
        for version in since..=latest {
            assert!(
                !reachable_at[&version].contains(node),
                "node stale since {since} is still reachable at {version}"
            );
        }
        assert!(
            stale.insert(node.to_vec()),
            "the same node has two stale records"
        );
    }
    for node in &stored {
        if !reachable_at[&latest].contains(node) {
            assert!(
                stale.contains(node),
                "node unreachable from the latest root has no stale record"
            );
        }
    }

    // Change records, value history and the latest index.
    let latest_index: BTreeMap<Vec<u8>, Vec<u8>> = entries(db, CF_LATEST).into_iter().collect();
    let mut preimages: BTreeMap<[u8; 32], Vec<u8>> = latest_index
        .keys()
        .map(|key| (key_hash(key), key.clone()))
        .collect();
    let mut changed = BTreeSet::new();
    for (entry, value) in entries(db, CF_CHANGES) {
        assert!(value.is_empty());
        let (version, key) = split_version(&entry).unwrap();
        assert!(
            version >= oldest && version <= latest,
            "change record {version} is not within [{oldest}, {latest}]"
        );
        preimages.insert(key_hash(key), key.to_vec());
        changed.insert((key_hash(key), version));
    }
    let mut history: History = BTreeMap::new();
    for (key, stored_value) in entries(db, CF_VALUES) {
        assert_eq!(key.len(), 40);
        let hash: [u8; 32] = key[..32].try_into().unwrap();
        let version = !u64::from_be_bytes(key[32..].try_into().unwrap());
        history
            .entry(hash)
            .or_default()
            .push((version, decode_stored_value(&stored_value).unwrap()));
    }
    for (hash, entries) in &history {
        // The value history is ordered from newest to oldest version.
        assert!(entries.windows(2).all(|w| w[0].0 > w[1].0));
        for (version, _) in entries {
            assert!(
                *version <= latest,
                "value history contains version {version} after latest"
            );
            if *version > oldest {
                assert!(
                    changed.contains(&(*hash, *version)),
                    "value of version {version} has no corresponding change record"
                );
            }
        }
        let old: Vec<_> = entries.iter().filter(|(v, _)| *v <= oldest).collect();
        assert!(
            old.len() <= 1,
            "old values <= oldest were not pruned: {old:?}"
        );
        if let Some((_, value)) = old.first() {
            assert!(value.is_some(), "deletion record <= oldest was not pruned");
        }
        let newest = entries.first().and_then(|(_, value)| value.clone());
        let key = preimages
            .get(hash)
            .expect("no preimage found for a key in the value history");
        assert_eq!(
            latest_index.get(key),
            newest.as_ref(),
            "latest index does not match the value history"
        );
        assert_eq!(read_value_at(db, None, hash, latest).unwrap(), newest);
    }
    for (hash, version) in &changed {
        assert!(
            history
                .get(hash)
                .is_some_and(|entries| entries.iter().any(|(v, _)| v == version)),
            "change record has no corresponding value"
        );
    }
    for key in latest_index.keys() {
        assert!(
            history.contains_key(&key_hash(key)),
            "key in the latest index has no value history"
        );
    }
}
