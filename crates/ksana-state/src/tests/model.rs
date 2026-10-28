//! Model test: random sequences of commit / rollback / prune / reopen / view operations, compared item by item
//! against an in-memory model (one `BTreeMap` per version) and the independent root reference implementation
//! (`oracle`), with a storage consistency check after every step.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{consistency, oracle};
use crate::{StateDb, StateError, StateRead, StateView, Version, WriteSet};

type State = BTreeMap<Vec<u8>, Vec<u8>>;

/// Key space: small enough to guarantee frequent overwrites and deletions; the keys under `0x05` are prefixes of
/// each other, used to check `scan_prefix`.
pub(crate) fn all_keys() -> Vec<Vec<u8>> {
    let mut keys = Vec::new();
    for prefix in [0x01u8, 0x02] {
        for b in 0..4u8 {
            keys.push(vec![prefix, b]);
        }
    }
    keys.push(vec![0x05]);
    for a in 0..3u8 {
        keys.push(vec![0x05, a]);
        for b in 0..3u8 {
            keys.push(vec![0x05, a, b]);
        }
    }
    keys
}

const PREFIXES: [&[u8]; 8] = [
    &[],
    &[0x01],
    &[0x02, 0x03],
    &[0x05],
    &[0x05, 0x00],
    &[0x05, 0x01, 0x02],
    &[0x06],
    &[0xff],
];

fn value() -> impl Strategy<Value = Vec<u8>> {
    // The value domain is tiny (including the empty value), so the current value is often written again.
    prop::collection::vec(0u8..3, 0..3)
}

pub(crate) fn write_set() -> impl Strategy<Value = WriteSet> {
    prop::collection::btree_map(
        prop::sample::select(all_keys()),
        prop::option::weighted(0.7, value()),
        0..6,
    )
}

pub(crate) fn genesis() -> impl Strategy<Value = Version> {
    prop_oneof![Just(0u64), Just(1), 2u64..1000, Just(u64::MAX - 64)]
}

#[derive(Clone, Debug)]
enum Op {
    Commit(WriteSet),
    /// Roll back this many versions (not below oldest).
    Rollback(u64),
    /// Prune to `latest - n` (may be below oldest, in which case it should be a no-op).
    Prune(u64),
    Reopen,
    /// Create one view each at the latest and the oldest version, checked after every later step.
    View,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => write_set().prop_map(Op::Commit),
        2 => (0u64..4).prop_map(Op::Rollback),
        2 => (0u64..6).prop_map(Op::Prune),
        1 => Just(Op::Reopen),
        1 => Just(Op::View),
    ]
}

pub(crate) fn apply(state: &mut State, writes: &WriteSet) {
    for (key, value) in writes {
        match value {
            Some(value) => state.insert(key.clone(), value.clone()),
            None => state.remove(key),
        };
    }
}

fn check_model(state: &mut StateDb, model: &BTreeMap<Version, State>) {
    let keys = all_keys();
    let (Some((&oldest, _)), Some((&latest, latest_state))) =
        (model.first_key_value(), model.last_key_value())
    else {
        assert_eq!(state.latest_version(), None);
        assert_eq!(state.oldest_version(), None);
        for key in &keys {
            assert_eq!(state.get(key).unwrap(), None);
        }
        assert_eq!(state.scan_prefix(&[]).unwrap(), vec![]);
        assert_eq!(state.root(0), Err(StateError::Empty));
        assert_eq!(state.get_at(&keys[0], 0), Err(StateError::Empty));
        assert_eq!(state.view().map(|_| ()), Err(StateError::Empty));
        return;
    };
    assert_eq!(state.oldest_version(), Some(oldest));
    assert_eq!(state.latest_version(), Some(latest));
    for (&version, expected) in model {
        assert_eq!(
            state.root(version).unwrap(),
            oracle::root(expected),
            "root of version {version}"
        );
        for key in &keys {
            assert_eq!(
                state.get_at(key, version).unwrap().as_ref(),
                expected.get(key),
                "get_at({key:?}, {version})"
            );
        }
    }
    let reader: &dyn StateRead = &*state;
    for key in &keys {
        assert_eq!(state.get(key).unwrap().as_ref(), latest_state.get(key));
        assert_eq!(reader.get(key).unwrap().as_ref(), latest_state.get(key));
    }
    for prefix in PREFIXES {
        let expected: Vec<(Vec<u8>, Vec<u8>)> = latest_state
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        assert_eq!(
            state.scan_prefix(prefix).unwrap(),
            expected,
            "scan_prefix({prefix:?})"
        );
    }
    let unavailable = [oldest.checked_sub(1), latest.checked_add(1)];
    for version in unavailable.into_iter().flatten() {
        let err = StateError::VersionUnavailable {
            version,
            oldest,
            latest,
        };
        assert_eq!(state.root(version), Err(err.clone()));
        assert_eq!(state.get_at(&keys[0], version), Err(err.clone()));
        assert_eq!(state.view_at(version).map(|_| ()), Err(err.clone()));
        // A failed rollback / prune changes nothing (the later checks would notice).
        assert_eq!(state.rollback_to(version), Err(err.clone()));
        if version > latest {
            assert_eq!(state.prune(version), Err(err));
        }
    }
}

/// A view reads the state at its creation time as long as its version is still kept and its content unchanged;
/// otherwise it returns `StaleView`.
/// (Same content ⇔ same root: after a rollback and a re-commit of a version with the same content, the view remains
/// valid and reads correct data.)
fn check_views(views: &[(StateView, State)], model: &BTreeMap<Version, State>) {
    for (view, expected) in views {
        let valid = model.get(&view.version()) == Some(expected);
        for key in all_keys() {
            let got = view.get(&key);
            if valid {
                assert_eq!(got.unwrap().as_ref(), expected.get(&key));
            } else {
                assert_eq!(
                    got,
                    Err(StateError::StaleView {
                        version: view.version()
                    })
                );
            }
        }
    }
}

fn run(genesis: Version, ops: Vec<Op>) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = StateDb::open(dir.path()).unwrap();
    let mut model: BTreeMap<Version, State> = BTreeMap::new();
    let mut views: Vec<(StateView, State)> = Vec::new();
    for op in ops {
        match op {
            Op::Commit(writes) => {
                let (version, mut next) = match model.last_key_value() {
                    Some((&latest, latest_state)) => (latest + 1, latest_state.clone()),
                    None => (genesis, State::new()),
                };
                apply(&mut next, &writes);
                let expected = oracle::root(&next);
                assert_eq!(state.preview_root(&writes).unwrap(), expected);
                assert_eq!(state.commit(version, &writes).unwrap(), expected);
                model.insert(version, next);
            }
            Op::Rollback(depth) => match (model.first_key_value(), model.last_key_value()) {
                (Some((&oldest, _)), Some((&latest, _))) => {
                    let target = latest.saturating_sub(depth).max(oldest);
                    state.rollback_to(target).unwrap();
                    model.retain(|&version, _| version <= target);
                }
                _ => assert_eq!(state.rollback_to(genesis), Err(StateError::Empty)),
            },
            Op::Prune(depth) => match model.last_key_value() {
                Some((&latest, _)) => {
                    let keep_from = latest.saturating_sub(depth);
                    state.prune(keep_from).unwrap();
                    model.retain(|&version, _| version >= keep_from);
                }
                None => assert_eq!(state.prune(genesis), Err(StateError::Empty)),
            },
            Op::Reopen => {
                // Views hold the RocksDB instance; the same path can only be reopened once all of them are released.
                views.clear();
                drop(state);
                state = StateDb::open(dir.path()).unwrap();
            }
            Op::View => {
                if let (Some((&oldest, oldest_state)), Some((_, latest_state))) =
                    (model.first_key_value(), model.last_key_value())
                {
                    views.push((state.view().unwrap(), latest_state.clone()));
                    views.push((state.view_at(oldest).unwrap(), oldest_state.clone()));
                }
            }
        }
        check_model(&mut state, &model);
        check_views(&views, &model);
        consistency::check(&state);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Random commit / rollback / prune / reopen / view sequences agree with the model.
    #[test]
    fn random_histories_match_model(
        genesis in genesis(),
        ops in prop::collection::vec(op(), 1..32),
    ) {
        run(genesis, ops);
    }

    /// The root is determined only by the final key-value set: the final root of any commit history equals the root
    /// obtained by committing the final set (plus deletions of non-existent keys) once in a fresh database, and also
    /// equals the reference implementation's root.
    #[test]
    fn root_depends_only_on_final_set(
        genesis in genesis(),
        history in prop::collection::vec(write_set(), 1..12),
        fresh_version in prop_oneof![Just(0u64), 1u64..1_000_000],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut state = StateDb::open(dir.path()).unwrap();
        let mut model = State::new();
        let mut version = genesis;
        for writes in &history {
            state.commit(version, writes).unwrap();
            apply(&mut model, writes);
            version += 1;
        }
        let root = state.root(version - 1).unwrap();
        prop_assert_eq!(root, oracle::root(&model));

        let fresh_dir = tempfile::tempdir().unwrap();
        let mut fresh = StateDb::open(fresh_dir.path()).unwrap();
        let writes: WriteSet = all_keys()
            .into_iter()
            .map(|key| {
                let value = model.get(&key).cloned();
                (key, value)
            })
            .collect();
        prop_assert_eq!(fresh.preview_root(&writes).unwrap(), root);
        prop_assert_eq!(fresh.commit(fresh_version, &writes).unwrap(), root);
        consistency::check(&state);
        consistency::check(&fresh);
    }
}
