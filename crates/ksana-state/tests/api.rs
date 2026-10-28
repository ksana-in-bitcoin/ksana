//! Integration tests of the public API: read views, thread safety and object safety, persistence across reopening,
//! `scan_prefix`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ksana_state::{StateDb, StateError, StateRead, StateView, Version, WriteSet};

fn ws(items: &[(&[u8], Option<&[u8]>)]) -> WriteSet {
    items
        .iter()
        .map(|(key, value)| (key.to_vec(), value.map(<[u8]>::to_vec)))
        .collect()
}

fn open(dir: &tempfile::TempDir) -> StateDb {
    StateDb::open(dir.path()).unwrap()
}

const _: () = {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<StateView>();
    assert_send_sync_static::<StateDb>();
    assert_send_sync_static::<Box<dyn StateRead>>();
    assert_send_sync_static::<StateError>();
};

#[test]
fn state_read_is_object_safe() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = open(&dir);
    state.commit(0, &ws(&[(b"k", Some(b"v0"))])).unwrap();
    let view = state.view().unwrap();
    state.commit(1, &ws(&[(b"k", Some(b"v1"))])).unwrap();

    let by_ref: [&dyn StateRead; 2] = [&state, &view];
    assert_eq!(by_ref[0].get(b"k").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(by_ref[1].get(b"k").unwrap(), Some(b"v0".to_vec()));
    let boxed: Box<dyn StateRead> = Box::new(view.clone());
    assert_eq!(boxed.get(b"k").unwrap(), Some(b"v0".to_vec()));
    let shared: Arc<dyn StateRead> = Arc::new(view);
    assert_eq!(shared.get(b"missing").unwrap(), None);
}

#[test]
fn view_keeps_its_version_while_db_moves_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = open(&dir);
    assert_eq!(state.view().map(|_| ()), Err(StateError::Empty));
    state
        .commit(7, &ws(&[(b"a", Some(b"1")), (b"b", Some(b"2"))]))
        .unwrap();
    let view = state.view().unwrap();
    assert_eq!(view.version(), 7);

    // A view can be moved to another thread and read concurrently with later commits.
    let reader = {
        let view = view.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                assert_eq!(view.get(b"a").unwrap(), Some(b"1".to_vec()));
                assert_eq!(view.get(b"c").unwrap(), None);
            }
        })
    };
    for version in 8..20u64 {
        state
            .commit(
                version,
                &ws(&[
                    (b"a", Some(&version.to_le_bytes())),
                    (b"b", None),
                    (b"c", Some(b"3")),
                ]),
            )
            .unwrap();
    }
    reader.join().unwrap();

    assert_eq!(view.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(view.get(b"b").unwrap(), Some(b"2".to_vec()));
    assert_eq!(view.get(b"c").unwrap(), None);
    let at_12 = state.view_at(12).unwrap();
    assert_eq!(at_12.version(), 12);
    assert_eq!(at_12.get(b"a").unwrap(), Some(12u64.to_le_bytes().to_vec()));
    assert_eq!(at_12.get(b"b").unwrap(), None);
    assert_eq!(
        state.view_at(20).map(|_| ()),
        Err(StateError::VersionUnavailable {
            version: 20,
            oldest: 7,
            latest: 19
        })
    );
}

#[test]
fn view_goes_stale_after_rollback_or_prune() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = open(&dir);
    for version in 0..6u64 {
        state
            .commit(version, &ws(&[(b"k", Some(&version.to_le_bytes()))]))
            .unwrap();
    }
    let v1 = state.view_at(1).unwrap();
    let v4 = state.view_at(4).unwrap();
    let v5 = state.view_at(5).unwrap();

    // Roll back to 3: the views of versions 4 and 5 become stale; after re-committing version 4 with the same content
    // as before, v4 is valid again (same content, same root), and after committing version 5 with different content
    // v5 remains stale.
    state.rollback_to(3).unwrap();
    assert_eq!(v4.get(b"k"), Err(StateError::StaleView { version: 4 }));
    assert_eq!(v5.get(b"k"), Err(StateError::StaleView { version: 5 }));
    state
        .commit(4, &ws(&[(b"k", Some(&4u64.to_le_bytes()))]))
        .unwrap();
    state.commit(5, &ws(&[(b"k", Some(b"fork"))])).unwrap();
    assert_eq!(v4.get(b"k").unwrap(), Some(4u64.to_le_bytes().to_vec()));
    assert_eq!(v5.get(b"k"), Err(StateError::StaleView { version: 5 }));

    // Prune to 2: the view of version 1 becomes stale.
    assert_eq!(v1.get(b"k").unwrap(), Some(1u64.to_le_bytes().to_vec()));
    state.prune(2).unwrap();
    assert_eq!(v1.get(b"k"), Err(StateError::StaleView { version: 1 }));
    assert_eq!(v4.get(b"k").unwrap(), Some(4u64.to_le_bytes().to_vec()));
}

/// Another thread reads the view repeatedly while the main thread repeatedly rolls back and re-commits version 5
/// (alternating between the original content and forked content): every read of the view is either version 5's
/// original value or `StaleView`, and never the forked data.
#[test]
fn concurrent_view_reads_never_see_another_branch() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = open(&dir);
    for version in 0..=5u64 {
        state
            .commit(
                version,
                &ws(&[(b"k", Some(&version.to_le_bytes())), (b"fixed", Some(b"f"))]),
            )
            .unwrap();
    }
    let view = state.view_at(5).unwrap();
    let original = 5u64.to_le_bytes().to_vec();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (view, stop, original) = (view.clone(), Arc::clone(&stop), original.clone());
        std::thread::spawn(move || {
            let mut reads = 0u64;
            while !stop.load(Ordering::Relaxed) || reads == 0 {
                match view.get(b"k") {
                    Ok(value) => assert_eq!(value.as_ref(), Some(&original)),
                    Err(err) => assert_eq!(err, StateError::StaleView { version: 5 }),
                }
                reads += 1;
            }
        })
    };
    for round in 0..40u64 {
        state.rollback_to(4).unwrap();
        let value = if round % 2 == 0 {
            original.clone()
        } else {
            format!("fork {round}").into_bytes()
        };
        state.commit(5, &ws(&[(b"k", Some(&value))])).unwrap();
        state.commit(6, &ws(&[(b"k", Some(b"later"))])).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    // The last round is a fork: the view is stale; once the same content is back, the view is valid again.
    assert_eq!(view.get(b"k"), Err(StateError::StaleView { version: 5 }));
    state.rollback_to(4).unwrap();
    state.commit(5, &ws(&[(b"k", Some(&original))])).unwrap();
    assert_eq!(view.get(b"k").unwrap(), Some(original));
    assert_eq!(view.get(b"fixed").unwrap(), Some(b"f".to_vec()));
}

/// Views hold the RocksDB instance: the same path can only be reopened once the `StateDb` and all views are released
/// (opening before that returns an error).
#[test]
fn views_keep_the_database_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = open(&dir);
    state.commit(0, &ws(&[(b"k", Some(b"v"))])).unwrap();
    let view = state.view().unwrap();
    drop(state);
    assert_eq!(view.get(b"k").unwrap(), Some(b"v".to_vec()));
    assert!(matches!(StateDb::open(dir.path()), Err(StateError::Db(_))));
    drop(view);
    let state = open(&dir);
    assert_eq!(state.get(b"k").unwrap(), Some(b"v".to_vec()));
}

/// After reopening, the version range, roots and values are unchanged (including after rollback and pruning), and
/// commits can continue.
#[test]
fn state_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut expected_roots: BTreeMap<Version, [u8; 32]> = BTreeMap::new();
    {
        let mut state = open(&dir);
        for version in 100..110u64 {
            let root = state
                .commit(
                    version,
                    &ws(&[
                        (b"counter", Some(&version.to_le_bytes())),
                        (&version.to_be_bytes(), Some(b"marker")),
                        (b"odd", (version % 2 == 1).then_some(b"yes")),
                    ]),
                )
                .unwrap();
            expected_roots.insert(version, root);
        }
        state.rollback_to(107).unwrap();
        state.prune(103).unwrap();
        expected_roots.retain(|version, _| (103..=107).contains(version));
    }
    let check = |state: &StateDb| {
        assert_eq!(state.oldest_version(), Some(103));
        assert_eq!(state.latest_version(), Some(107));
        for (&version, root) in &expected_roots {
            assert_eq!(state.root(version).unwrap(), *root);
            assert_eq!(
                state.get_at(b"counter", version).unwrap(),
                Some(version.to_le_bytes().to_vec())
            );
            assert_eq!(
                state.get_at(&version.to_be_bytes(), version).unwrap(),
                Some(b"marker".to_vec())
            );
            assert_eq!(
                state.get_at(b"odd", version).unwrap().is_some(),
                version % 2 == 1
            );
        }
        assert_eq!(state.get(&108u64.to_be_bytes()).unwrap(), None);
        assert!(state.root(102).is_err());
        assert!(state.root(108).is_err());
    };
    let mut state = open(&dir);
    check(&state);
    drop(state);
    state = open(&dir);
    check(&state);
    state.commit(108, &ws(&[(b"counter", None)])).unwrap();
    drop(state);
    let state = open(&dir);
    assert_eq!(state.latest_version(), Some(108));
    assert_eq!(state.get(b"counter").unwrap(), None);
    assert_eq!(
        state.get_at(b"counter", 107).unwrap(),
        Some(107u64.to_le_bytes().to_vec())
    );
}

#[test]
fn scan_prefix_returns_latest_values_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = open(&dir);
    assert_eq!(state.scan_prefix(b"").unwrap(), vec![]);
    state
        .commit(
            0,
            &ws(&[
                (b"\x02acct\x02", Some(b"b2")),
                (b"\x02acct\x01", Some(b"b1")),
                (b"\x02acct", Some(b"b0")),
                (b"\x02other", Some(b"o")),
                (b"\x01acct", Some(b"n")),
                (b"\xff\xff", Some(b"max")),
                (b"\xff\xff\x00", Some(b"max0")),
            ]),
        )
        .unwrap();
    state
        .commit(
            1,
            &ws(&[(b"\x02acct\x01", None), (b"\x02acct\x02", Some(b"b2-new"))]),
        )
        .unwrap();
    let pairs = |items: &[(&[u8], &[u8])]| -> Vec<(Vec<u8>, Vec<u8>)> {
        items
            .iter()
            .map(|(key, value)| (key.to_vec(), value.to_vec()))
            .collect()
    };
    assert_eq!(
        state.scan_prefix(b"\x02acct").unwrap(),
        pairs(&[(b"\x02acct", b"b0"), (b"\x02acct\x02", b"b2-new")])
    );
    assert_eq!(
        state.scan_prefix(b"\x02").unwrap(),
        pairs(&[
            (b"\x02acct", b"b0"),
            (b"\x02acct\x02", b"b2-new"),
            (b"\x02other", b"o"),
        ])
    );
    assert_eq!(
        state.scan_prefix(b"\xff\xff").unwrap(),
        pairs(&[(b"\xff\xff", b"max"), (b"\xff\xff\x00", b"max0")])
    );
    assert_eq!(state.scan_prefix(b"\x03").unwrap(), vec![]);
    assert_eq!(state.scan_prefix(b"").unwrap().len(), 6);

    state.rollback_to(0).unwrap();
    assert_eq!(
        state.scan_prefix(b"\x02acct").unwrap(),
        pairs(&[
            (b"\x02acct", b"b0"),
            (b"\x02acct\x01", b"b1"),
            (b"\x02acct\x02", b"b2"),
        ])
    );
}

/// A version with no writes (or only writes of values equal to the current ones) leaves the state root unchanged
/// (D-079: an empty block's state root equals the selected parent's); reads, rollback and pruning work as usual
/// afterwards.
#[test]
fn versions_without_changes_keep_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = open(&dir);
    let root = state
        .commit(0, &ws(&[(b"a", Some(b"1")), (b"b", Some(b"2"))]))
        .unwrap();
    assert_eq!(state.preview_root(&WriteSet::new()).unwrap(), root);
    assert_eq!(state.commit(1, &WriteSet::new()).unwrap(), root);
    assert_eq!(state.commit(2, &ws(&[(b"a", Some(b"1"))])).unwrap(), root);
    assert_eq!(state.commit(3, &WriteSet::new()).unwrap(), root);
    let changed = state.commit(4, &ws(&[(b"a", Some(b"9"))])).unwrap();
    assert_ne!(changed, root);
    assert_eq!(state.commit(5, &WriteSet::new()).unwrap(), changed);
    assert_eq!(
        state.view().unwrap().get(b"a").unwrap(),
        Some(b"9".to_vec())
    );
    state.rollback_to(3).unwrap();
    assert_eq!(state.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(state.commit(4, &WriteSet::new()).unwrap(), root);
    state.prune(2).unwrap();
    assert_eq!(
        state.view().unwrap().get(b"b").unwrap(),
        Some(b"2".to_vec())
    );
    assert_ne!(state.commit(5, &ws(&[(b"b", None)])).unwrap(), root);
}
