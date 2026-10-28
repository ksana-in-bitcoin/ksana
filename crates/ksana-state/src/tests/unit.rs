//! Unit tests of specific behaviors (jmt edge cases, no-ops, pruning really deleting data, etc.).

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::consistency::{self, entries, reachable};
use super::model::{self, apply};
use super::oracle::{self, PLACEHOLDER};
use crate::schema::{CF_CHANGES, CF_NODES, CF_ROOTS, CF_STALE, CF_VALUES, COLUMN_FAMILIES};
use crate::{StateDb, StateError, StateRead, Version, WriteSet};

fn open() -> (tempfile::TempDir, StateDb) {
    let dir = tempfile::tempdir().unwrap();
    let state = StateDb::open(dir.path()).unwrap();
    (dir, state)
}

fn ws(items: &[(&[u8], Option<&[u8]>)]) -> WriteSet {
    items
        .iter()
        .map(|(key, value)| (key.to_vec(), value.map(<[u8]>::to_vec)))
        .collect()
}

fn count(state: &StateDb, cf: &str) -> usize {
    entries(state.raw(), cf).len()
}

fn snapshot_all(state: &StateDb) -> Vec<Vec<(Vec<u8>, Vec<u8>)>> {
    COLUMN_FAMILIES
        .iter()
        .map(|cf| entries(state.raw(), cf))
        .collect()
}

#[test]
fn empty_write_set_creates_version_with_same_root() {
    let (_dir, mut state) = open();
    let root3 = state
        .commit(3, &ws(&[(b"a", Some(b"1")), (b"b", Some(b"2"))]))
        .unwrap();
    let root4 = state.commit(4, &WriteSet::new()).unwrap();
    assert_eq!(root4, root3);
    assert_eq!(state.root(4).unwrap(), root3);
    assert_eq!(state.latest_version(), Some(4));
    assert_eq!(state.get_at(b"a", 4).unwrap(), Some(b"1".to_vec()));
    consistency::check(&state);
    // After an empty version, commits continue normally, and it (or anything before it) can be rolled back to.
    state.commit(5, &ws(&[(b"a", None)])).unwrap();
    state.rollback_to(4).unwrap();
    assert_eq!(state.get(b"a").unwrap(), Some(b"1".to_vec()));
    state.rollback_to(3).unwrap();
    consistency::check(&state);
}

/// When an empty database starts at genesis `g > 0` with no change at all, jmt would write the empty root at version
/// `g - 1`; this verifies the corrected behavior.
#[test]
fn genesis_without_changes_at_any_version() {
    for genesis in [0, 1, 7, 1 << 40, u64::MAX - 1] {
        for writes in [WriteSet::new(), ws(&[(b"absent", None), (b"x", None)])] {
            let (_dir, mut state) = open();
            assert_eq!(state.preview_root(&writes).unwrap(), PLACEHOLDER);
            assert_eq!(state.commit(genesis, &writes).unwrap(), PLACEHOLDER);
            assert_eq!(state.root(genesis).unwrap(), PLACEHOLDER);
            assert_eq!(state.oldest_version(), Some(genesis));
            assert_eq!(state.latest_version(), Some(genesis));
            consistency::check(&state);
            if genesis == u64::MAX - 1 {
                assert_eq!(
                    state.commit(u64::MAX, &writes),
                    Err(StateError::VersionOutOfRange(u64::MAX))
                );
                continue;
            }
            let root = state
                .commit(genesis + 1, &ws(&[(b"k", Some(b"v"))]))
                .unwrap();
            assert_eq!(
                root,
                oracle::root(&BTreeMap::from([(b"k".to_vec(), b"v".to_vec())]))
            );
            let root = state.commit(genesis + 2, &ws(&[(b"k", None)])).unwrap();
            assert_eq!(root, PLACEHOLDER);
            consistency::check(&state);
            state.prune(genesis + 2).unwrap();
            consistency::check(&state);
        }
    }
}

#[test]
fn genesis_with_data_at_nonzero_version() {
    let (_dir, mut state) = open();
    let writes = ws(&[(b"a", Some(b"1")), (b"gone", None)]);
    let root = state.commit(42, &writes).unwrap();
    assert_eq!(
        root,
        oracle::root(&BTreeMap::from([(b"a".to_vec(), b"1".to_vec())]))
    );
    assert_eq!(state.get_at(b"a", 42).unwrap(), Some(b"1".to_vec()));
    consistency::check(&state);
}

/// Deleting a non-existent key, writing the same value: the root is unchanged, and no value history, change records
/// or rebuilt nodes are written (only the new version's root node and the old root's stale record).
#[test]
fn noop_writes_write_nothing() {
    let (_dir, mut state) = open();
    let root = state
        .commit(
            0,
            &ws(&[(b"a", Some(b"1")), (b"b", Some(b"")), (b"c", Some(b"3"))]),
        )
        .unwrap();
    let before = [CF_VALUES, CF_CHANGES, CF_NODES, CF_STALE, CF_ROOTS].map(|cf| count(&state, cf));
    let noop = ws(&[
        (b"a", Some(b"1")),
        (b"b", Some(b"")),
        (b"absent", None),
        (b"", None),
    ]);
    assert_eq!(state.preview_root(&noop).unwrap(), root);
    assert_eq!(state.commit(1, &noop).unwrap(), root);
    let after = [CF_VALUES, CF_CHANGES, CF_NODES, CF_STALE, CF_ROOTS].map(|cf| count(&state, cf));
    assert_eq!(after[0], before[0], "value history should not grow");
    assert_eq!(after[1], before[1], "change records should not grow");
    assert_eq!(
        after[2],
        before[2] + 1,
        "only the root node of version 1 should be added"
    );
    assert_eq!(
        after[3],
        before[3] + 1,
        "only the stale record of the old root should be added"
    );
    assert_eq!(after[4], before[4] + 1);
    consistency::check(&state);

    // Real changes mixed with no-ops: only the keys that really change enter the change records.
    let mixed = ws(&[(b"a", Some(b"1")), (b"c", None), (b"absent", None)]);
    state.commit(2, &mixed).unwrap();
    assert_eq!(count(&state, CF_CHANGES), before[1] + 1);
    consistency::check(&state);
}

#[test]
fn empty_value_is_not_absence() {
    let (_dir, mut state) = open();
    let root = state.commit(0, &ws(&[(b"k", Some(b""))])).unwrap();
    assert_ne!(root, PLACEHOLDER);
    assert_eq!(
        root,
        oracle::root(&BTreeMap::from([(b"k".to_vec(), Vec::new())]))
    );
    assert_eq!(state.get(b"k").unwrap(), Some(Vec::new()));
    assert_eq!(state.get_at(b"k", 0).unwrap(), Some(Vec::new()));
    assert_eq!(
        state.scan_prefix(b"k").unwrap(),
        vec![(b"k".to_vec(), Vec::new())]
    );
    // The empty key is a valid key too.
    state.commit(1, &ws(&[(b"", Some(b"e"))])).unwrap();
    assert_eq!(state.get(b"").unwrap(), Some(b"e".to_vec()));
    assert_eq!(
        state.commit(2, &ws(&[(b"k", None), (b"", None)])).unwrap(),
        PLACEHOLDER
    );
    assert_eq!(state.get(b"k").unwrap(), None);
    assert_eq!(state.get_at(b"k", 1).unwrap(), Some(Vec::new()));
    consistency::check(&state);
}

#[test]
fn commit_version_rules() {
    let (_dir, mut state) = open();
    assert_eq!(
        state.commit(u64::MAX, &WriteSet::new()),
        Err(StateError::VersionOutOfRange(u64::MAX))
    );
    state.commit(10, &ws(&[(b"a", Some(b"1"))])).unwrap();
    for wrong in [0, 9, 10, 12, u64::MAX] {
        assert_eq!(
            state.commit(wrong, &WriteSet::new()),
            Err(StateError::UnexpectedVersion {
                expected: 11,
                got: wrong
            })
        );
    }
    assert_eq!(state.latest_version(), Some(10));
    state.commit(11, &WriteSet::new()).unwrap();
    consistency::check(&state);
}

#[test]
fn rollback_then_commit_different_content() {
    let (_dir, mut state) = open();
    let mut model: BTreeMap<Version, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
    let mut current = BTreeMap::new();
    for version in 1..=5u64 {
        let v = version.to_be_bytes();
        let writes = ws(&[
            (b"shared", Some(&v)),
            (&v, Some(b"x")),
            (b"flip", (version % 2 == 0).then_some(b"on")),
        ]);
        apply(&mut current, &writes);
        state.commit(version, &writes).unwrap();
        model.insert(version, current.clone());
    }
    let old_root3 = state.root(3).unwrap();
    state.rollback_to(2).unwrap();
    assert_eq!(state.latest_version(), Some(2));
    assert_eq!(state.root(2).unwrap(), oracle::root(&model[&2]));
    for version in 3..=5 {
        assert!(matches!(
            state.root(version),
            Err(StateError::VersionUnavailable { .. })
        ));
        assert!(state.get_at(b"shared", version).is_err());
    }
    assert_eq!(
        state.get(b"shared").unwrap(),
        Some(2u64.to_be_bytes().to_vec())
    );
    assert_eq!(state.get(b"flip").unwrap(), Some(b"on".to_vec()));
    assert_eq!(state.get(&3u64.to_be_bytes()).unwrap(), None);
    consistency::check(&state);

    let mut next = model[&2].clone();
    let writes = ws(&[
        (b"shared", Some(b"other")),
        (b"flip", None),
        (b"new", Some(b"n")),
    ]);
    apply(&mut next, &writes);
    let root3 = state.commit(3, &writes).unwrap();
    assert_ne!(root3, old_root3);
    assert_eq!(root3, oracle::root(&next));
    assert_eq!(state.get_at(b"shared", 3).unwrap(), Some(b"other".to_vec()));
    assert_eq!(
        state.get_at(b"shared", 2).unwrap(),
        Some(2u64.to_be_bytes().to_vec())
    );
    assert_eq!(state.get_at(b"new", 2).unwrap(), None);
    consistency::check(&state);
}

/// Pruning really deletes the superseded nodes and values: after pruning to the latest version, the database holds
/// only the nodes of the latest tree and one value per live key.
#[test]
fn prune_deletes_superseded_nodes_and_values() {
    let (_dir, mut state) = open();
    let keys: Vec<Vec<u8>> = (0..40u8).map(|i| vec![0x05, i]).collect();
    for version in 0..30u64 {
        let mut writes = WriteSet::new();
        for (i, key) in keys.iter().enumerate() {
            let i = i as u64;
            if (i + version).is_multiple_of(3) {
                writes.insert(key.clone(), Some(version.to_le_bytes().to_vec()));
            } else if (i + version).is_multiple_of(7) {
                writes.insert(key.clone(), None);
            }
        }
        state.commit(version, &writes).unwrap();
    }
    let latest = 29;
    let expected: BTreeMap<Vec<u8>, Vec<u8>> = keys
        .iter()
        .filter_map(|key| Some((key.clone(), state.get(key).unwrap()?)))
        .collect();
    let nodes_before = count(&state, CF_NODES);
    let values_before = count(&state, CF_VALUES);

    state.prune(20).unwrap();
    assert_eq!(state.oldest_version(), Some(20));
    assert!(count(&state, CF_NODES) < nodes_before);
    assert!(count(&state, CF_VALUES) < values_before);
    consistency::check(&state);
    for version in 20..=latest {
        state.root(version).unwrap();
    }
    assert!(matches!(
        state.root(19),
        Err(StateError::VersionUnavailable { .. })
    ));

    state.prune(latest).unwrap();
    consistency::check(&state);
    assert_eq!(
        count(&state, CF_NODES),
        reachable(state.raw(), latest).len()
    );
    assert_eq!(count(&state, CF_VALUES), expected.len());
    assert_eq!(count(&state, CF_CHANGES), 0);
    assert_eq!(count(&state, CF_STALE), 0);
    assert_eq!(count(&state, CF_ROOTS), 1);
    assert_eq!(state.root(latest).unwrap(), oracle::root(&expected));
    for (key, value) in &expected {
        assert_eq!(state.get_at(key, latest).unwrap().as_ref(), Some(value));
    }
    state.rollback_to(latest).unwrap();
    assert!(state.rollback_to(latest - 1).is_err());
    // Commit again after pruning, then prune again.
    state.commit(latest + 1, &ws(&[(&keys[0], None)])).unwrap();
    state.prune(latest + 1).unwrap();
    consistency::check(&state);
}

/// Pruning to k and then rolling back to k: the state, root, `get` / `get_at` / `scan_prefix` are exactly those of
/// version k before pruning, and a new branch can then be committed from k + 1.
#[test]
fn rollback_to_prune_boundary_restores_exact_state() {
    let (_dir, mut state) = open();
    let keys: Vec<Vec<u8>> = (0..16u8).map(|i| vec![0x02, i]).collect();
    let mut model: BTreeMap<Version, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
    let mut current = BTreeMap::new();
    for version in 0..12u64 {
        let mut writes = WriteSet::new();
        for (i, key) in keys.iter().enumerate() {
            match (i as u64 * 7 + version * 3) % 5 {
                0 => {
                    writes.insert(key.clone(), Some(vec![u8::try_from(version).unwrap(); 2]));
                }
                1 => {
                    writes.insert(key.clone(), None);
                }
                _ => {}
            }
        }
        apply(&mut current, &writes);
        state.commit(version, &writes).unwrap();
        model.insert(version, current.clone());
    }
    state.prune(6).unwrap();
    state.rollback_to(6).unwrap();
    consistency::check(&state);
    let expected = &model[&6];
    assert_eq!(state.oldest_version(), Some(6));
    assert_eq!(state.latest_version(), Some(6));
    assert_eq!(state.root(6).unwrap(), oracle::root(expected));
    for key in &keys {
        assert_eq!(state.get(key).unwrap().as_ref(), expected.get(key));
        assert_eq!(state.get_at(key, 6).unwrap().as_ref(), expected.get(key));
    }
    let scanned: BTreeMap<Vec<u8>, Vec<u8>> =
        state.scan_prefix(&[0x02]).unwrap().into_iter().collect();
    assert_eq!(&scanned, expected);

    let mut next = expected.clone();
    let writes: WriteSet = keys
        .iter()
        .map(|key| (key.clone(), Some(b"branch".to_vec())))
        .collect();
    apply(&mut next, &writes);
    assert_eq!(state.commit(7, &writes).unwrap(), oracle::root(&next));
    consistency::check(&state);
}

#[test]
fn prune_and_rollback_errors() {
    let (_dir, mut state) = open();
    assert_eq!(state.prune(0), Err(StateError::Empty));
    assert_eq!(state.rollback_to(0), Err(StateError::Empty));
    for version in 5..=8u64 {
        let value = [u8::try_from(version).unwrap()];
        state.commit(version, &ws(&[(b"k", Some(&value))])).unwrap();
    }
    let unavailable = |version| StateError::VersionUnavailable {
        version,
        oldest: 5,
        latest: 8,
    };
    assert_eq!(state.prune(9), Err(unavailable(9)));
    assert_eq!(state.rollback_to(4), Err(unavailable(4)));
    assert_eq!(state.rollback_to(9), Err(unavailable(9)));
    // keep_from <= oldest: no-op.
    let before = snapshot_all(&state);
    state.prune(0).unwrap();
    state.prune(5).unwrap();
    state.rollback_to(8).unwrap();
    assert_eq!(snapshot_all(&state), before);
    state.prune(7).unwrap();
    let unavailable = |version| StateError::VersionUnavailable {
        version,
        oldest: 7,
        latest: 8,
    };
    assert_eq!(state.rollback_to(6), Err(unavailable(6)));
    assert_eq!(state.get_at(b"k", 6), Err(unavailable(6)));
    assert_eq!(state.get_at(b"k", 7).unwrap(), Some(vec![7]));
    consistency::check(&state);
}

/// `reset` clears all column families and returns to an empty database: old views report staleness on read, and
/// genesis can be created again afterwards with different content (D-093).
#[test]
fn reset_empties_everything_and_allows_a_new_genesis() {
    let (_dir, mut state) = open();
    for version in 0..6u8 {
        let key = format!("k{version}");
        state
            .commit(
                u64::from(version),
                &ws(&[(key.as_bytes(), Some(b"v")), (b"shared", Some(&[version]))]),
            )
            .unwrap();
    }
    state.prune(3).unwrap();
    let view = state.view_at(4).unwrap();
    assert_eq!(view.get(b"shared").unwrap(), Some(vec![4]));

    state.reset().unwrap();
    assert_eq!(state.latest_version(), None);
    assert_eq!(state.oldest_version(), None);
    for cf in COLUMN_FAMILIES {
        assert_eq!(count(&state, cf), 0, "column family {cf} should be empty");
    }
    assert_eq!(state.get(b"shared").unwrap(), None);
    assert_eq!(
        view.get(b"shared").unwrap_err(),
        StateError::StaleView { version: 4 }
    );

    // Genesis again at any version; the content and the root depend only on the new key-values
    let writes = ws(&[(b"g", Some(b"1"))]);
    let expected = oracle::root(&BTreeMap::from([(b"g".to_vec(), b"1".to_vec())]));
    assert_eq!(state.commit(0, &writes).unwrap(), expected);
    assert_eq!(state.get(b"k1").unwrap(), None);
    assert_eq!(state.get(b"g").unwrap(), Some(b"1".to_vec()));
    consistency::check(&state);
    // Resetting again after a reset (empty database) also succeeds
    state.reset().unwrap();
    assert_eq!(state.latest_version(), None);
    state.commit(7, &writes).unwrap();
    consistency::check(&state);
}

#[test]
fn preview_root_writes_nothing() {
    let (_dir, mut state) = open();
    let writes = ws(&[(b"a", Some(b"1")), (b"b", None)]);
    let expected = oracle::root(&BTreeMap::from([(b"a".to_vec(), b"1".to_vec())]));
    assert_eq!(state.preview_root(&writes).unwrap(), expected);
    assert!(snapshot_all(&state).iter().all(Vec::is_empty));
    state.commit(0, &writes).unwrap();
    let before = snapshot_all(&state);
    let more = ws(&[(b"a", None), (b"c", Some(b"3"))]);
    let preview = state.preview_root(&more).unwrap();
    assert_eq!(snapshot_all(&state), before);
    assert_eq!(state.commit(1, &more).unwrap(), preview);
}

/// D-101: the root of committing on top of an older version equals the root of committing after rolling back to it,
/// with nothing written and the newer versions left as they are.
#[test]
fn preview_root_at_an_older_version() {
    let w1 = ws(&[(b"a", Some(b"1")), (b"b", Some(b"2"))]);
    let w2 = ws(&[(b"a", Some(b"9")), (b"c", Some(b"3"))]);
    let w3 = ws(&[(b"b", None), (b"d", Some(b"4"))]);
    // the branch from version 1: a no-op on `a`, a deletion, a deletion of a missing key and a new key
    let fork = ws(&[
        (b"a", Some(b"1")),
        (b"b", None),
        (b"absent", None),
        (b"e", Some(b"5")),
    ]);
    let (_dir, mut state) = open();
    state.commit(0, &WriteSet::new()).unwrap();
    state.commit(1, &w1).unwrap();
    state.commit(2, &w2).unwrap();
    state.commit(3, &w3).unwrap();
    let (_other_dir, mut other) = open();
    other.commit(0, &WriteSet::new()).unwrap();
    other.commit(1, &w1).unwrap();
    let expected = other.preview_root(&fork).unwrap();
    assert_eq!(other.commit(2, &fork).unwrap(), expected);

    let before = snapshot_all(&state);
    assert_eq!(state.preview_root_at(1, &fork).unwrap(), expected);
    assert_eq!(snapshot_all(&state), before);
    assert_eq!(state.latest_version(), Some(3));
    // at the latest version it is the plain preview
    assert_eq!(
        state.preview_root_at(3, &fork).unwrap(),
        state.preview_root(&fork).unwrap()
    );
    // the result is the same after the versions above were rolled back
    state.rollback_to(1).unwrap();
    assert_eq!(state.preview_root_at(1, &fork).unwrap(), expected);
    assert_eq!(state.commit(2, &fork).unwrap(), expected);
    consistency::check(&state);
    // outside the available range
    assert!(matches!(
        state.preview_root_at(5, &fork),
        Err(StateError::VersionUnavailable { .. })
    ));
    state.commit(3, &w3).unwrap();
    state.prune(2).unwrap();
    assert!(matches!(
        state.preview_root_at(1, &fork),
        Err(StateError::VersionUnavailable { .. })
    ));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]

    /// Larger trees (hundreds of random keys, the JMT has several levels of internal nodes): after random updates /
    /// deletions (including the merging of internal nodes once emptied), rollbacks and prunings, the root agrees with
    /// the reference implementation and the storage is consistent.
    #[test]
    fn larger_trees_match_oracle(
        seed_keys in prop::collection::btree_set(prop::collection::vec(any::<u8>(), 1..40), 200..400),
        rounds in prop::collection::vec((any::<u64>(), 0u8..4), 4..8),
    ) {
        let keys: Vec<Vec<u8>> = seed_keys.into_iter().collect();
        let (_dir, mut state) = open();
        let mut history: BTreeMap<Version, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
        let mut current = BTreeMap::new();
        for (round, (salt, action)) in rounds.iter().enumerate() {
            let version = round as u64;
            let mut writes = WriteSet::new();
            for (i, key) in keys.iter().enumerate() {
                let h = salt.rotate_left(u32::try_from(i % 64).unwrap())
                    ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                let len = usize::try_from(h % 9).unwrap();
                match (action, h % 5) {
                    // Delete most keys to trigger the merging of internal nodes.
                    (3, 0..=3) => { writes.insert(key.clone(), None); }
                    (_, 0) => { writes.insert(key.clone(), Some(h.to_le_bytes()[..len].to_vec())); }
                    (_, 1) => { writes.insert(key.clone(), None); }
                    _ => {}
                }
            }
            apply(&mut current, &writes);
            let root = state.commit(version, &writes).unwrap();
            prop_assert_eq!(root, oracle::root(&current));
            history.insert(version, current.clone());
        }
        consistency::check(&state);
        let latest = *history.keys().last().unwrap();
        state.rollback_to(latest - 2).unwrap();
        current = history[&(latest - 2)].clone();
        prop_assert_eq!(state.root(latest - 2).unwrap(), oracle::root(&current));
        let writes: WriteSet = keys.iter().take(50).map(|key| (key.clone(), Some(b"z".to_vec()))).collect();
        apply(&mut current, &writes);
        prop_assert_eq!(state.commit(latest - 1, &writes).unwrap(), oracle::root(&current));
        state.prune(latest - 2).unwrap();
        consistency::check(&state);
        for key in keys.iter().step_by(7) {
            let got = state.get(key).unwrap();
            prop_assert_eq!(got.as_ref(), current.get(key));
        }
    }
}

#[test]
fn model_key_space_is_distinct() {
    let keys = model::all_keys();
    let unique: std::collections::BTreeSet<_> = keys.iter().collect();
    assert_eq!(unique.len(), keys.len());
}
