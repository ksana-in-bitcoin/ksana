//! Property test (SPEC §15): executing a random entry sequence twice yields the same `state_root`.
//!
//! Each case randomly generates a number of blocks, each containing path-C and path-B transfers and unlock entries
//! (amounts, fees and recipients are random; some transfers are bound to fail, some blocks are invalid because of an
//! insufficient fee or unmet unlock conditions). Unlock entries use the deterministic mock BTC view [`SeqBtc`]. The
//! same sequence is executed under two engine configurations (parallel compilation on / off) on two fresh
//! RocksDB + JMT stores: the execution result of every block (invalidity reason or receipts) and the `state_root`
//! after committing must be identical.

mod common;

use std::sync::Arc;

use common::*;
use ksana_execution::keys::{self, AccountState, UnlockCursor};
use ksana_execution::{BlockEnv, BtcView, Executor, GenesisConfig, StateSource, UnlockItem};
use ksana_primitives::hash::sha256;
use ksana_primitives::{BtcAnchor, Entry, OutPoint, VmOp, DEVNET};
use ksana_state::StateDb;
use ksana_vm::Vm;
use proptest::prelude::*;

const USERS: usize = 4;

/// Deterministic mock BTC view: block `h` has entered the settled closure iff the first byte of `sha256(h)` is not a
/// multiple of 4 (about three quarters of the blocks).
struct SeqBtc;

impl BtcView for SeqBtc {
    fn header(&self, _height: u32) -> Result<Option<[u8; 80]>, String> {
        Ok(None)
    }

    fn block_settled(&self, block_hash: &[u8; 32]) -> Result<bool, String> {
        Ok(!sha256(block_hash)[0].is_multiple_of(4))
    }

    fn carrier(&self, block_hash: &[u8; 32]) -> Result<Option<[u8; 32]>, String> {
        // About half of the blocks have a carrier: when the second byte of the hash is even, the carrier is the account
        // determined by that byte (D-095)
        let byte = sha256(block_hash)[1];
        Ok(byte.is_multiple_of(2).then_some([byte; 32]))
    }

    fn native_input(&self, txid: &[u8; 32], _op: &VmOp) -> Result<Option<OutPoint>, String> {
        Ok(Some(test_first_input(txid)))
    }

    fn native_outputs(&self, _txid: &[u8; 32]) -> Result<Vec<(u64, Vec<u8>)>, String> {
        Err("ops in the random sequences do not read native transactions".into())
    }
}

/// Kind of entry.
#[derive(Clone, Debug)]
enum Kind {
    /// Path C.
    Forced,
    /// Path B with sponsored fee `fee`.
    Sponsored { fee: u128 },
    /// Unlock: when `bogus`, unlock a position not in the unlockable segment; otherwise pick one from the segment by
    /// `pick` (no entry when the segment is empty), which the view may consider not yet in the settled closure.
    Unlock { pick: usize, bogus: bool },
}

/// The intent of one random entry; the nonce and the block to unlock are filled in at execution time from the current
/// state.
#[derive(Clone, Debug)]
struct Intent {
    kind: Kind,
    from: usize,
    to: usize,
    amount: u128,
}

fn kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        3 => Just(Kind::Forced),
        3 => prop_oneof![9 => 0u128..20, 1 => Just(2_000_000u128)]
            .prop_map(|fee| Kind::Sponsored { fee }),
        2 => (0usize..4, prop::bool::weighted(0.1))
            .prop_map(|(pick, bogus)| Kind::Unlock { pick, bogus }),
    ]
}

fn intent() -> impl Strategy<Value = Intent> {
    (
        kind(),
        0..USERS,
        0..USERS,
        prop_oneof![4 => 0u128..5_000, 1 => Just(5_000_000u128)],
    )
        .prop_map(|(kind, from, to, amount)| Intent {
            kind,
            from,
            to,
            amount,
        })
}

fn blocks() -> impl Strategy<Value = Vec<Vec<Intent>>> {
    prop::collection::vec(prop::collection::vec(intent(), 0..8), 1..8)
}

/// The entry's phase within the block: forced inclusion, unlock, path B (SPEC §6.2).
fn phase(kind: &Kind) -> u8 {
    match kind {
        Kind::Forced => 0,
        Kind::Unlock { .. } => 1,
        Kind::Sponsored { .. } => 2,
    }
}

/// Per-block result: a summary of the execution result (invalidity reason or receipts) and the `state_root` afterwards.
fn run(vm: Vm, blocks: &[Vec<Intent>]) -> Vec<(String, [u8; 32])> {
    let users: Vec<User> = (0..USERS).map(|i| user(&format!("u{i}"))).collect();
    let dir = tempfile::tempdir().unwrap();
    let mut db = StateDb::open(dir.path()).unwrap();
    let genesis = GenesisConfig::devnet(users.iter().map(|u| (u.account, 1_000_000)).collect());
    db.commit(0, &genesis.writes().unwrap()).unwrap();
    let exec = Executor::with_vm(DEVNET, vm);
    let mut results = Vec::new();
    // The committed blocks (from height 1, none merging other blocks, acceptance position (height, 0)), used to give
    // the unlockable segment (D-079)
    let mut main: Vec<UnlockItem> = Vec::new();
    for (i, intents) in blocks.iter().enumerate() {
        let view = db.view().unwrap();
        let read = |key: &[u8]| view.get(key).unwrap();
        let nonce_of = |u: &User| -> u64 {
            read(&keys::account(&u.account))
                .map(|v| borsh::from_slice::<AccountState>(&v).unwrap().nonce)
                .unwrap_or(0)
        };
        let cursor = read(&keys::unlock_cursor())
            .map(|v| borsh::from_slice::<UnlockCursor>(&v).unwrap())
            .map(|c| (c.height, c.index));
        let max = usize::try_from(DEVNET.max_unlock_batch).unwrap();
        let segment: Vec<UnlockItem> = main
            .iter()
            .filter(|item| cursor.is_none_or(|c| item.position() > c))
            .take(max)
            .cloned()
            .collect();
        let height = db.latest_version().unwrap() + 1;

        // Forced inclusion first, then unlock, then path B (block structure rules);
        // nonces are filled in assuming "all earlier entries succeed"; mispredicted entries are executed anyway
        let mut ordered: Vec<&Intent> = intents.iter().collect();
        ordered.sort_by_key(|t| phase(&t.kind));
        let mut next: Vec<u64> = users.iter().map(nonce_of).collect();
        // At most one unlock entry per block (D-074): extra unlock intents are dropped
        let mut has_unlock = false;
        ordered.retain(|t| {
            let first = !(matches!(t.kind, Kind::Unlock { .. }) && has_unlock);
            has_unlock |= matches!(t.kind, Kind::Unlock { .. });
            first
        });
        let entries: Vec<Entry> = ordered
            .iter()
            .filter_map(|t| {
                let (from, to) = (&users[t.from], users[t.to].account);
                let action = transfer(to, t.amount);
                let mut nonce = || {
                    let n = next[t.from];
                    next[t.from] += 1;
                    n
                };
                Some(match &t.kind {
                    Kind::Forced => forced(from, nonce(), 100_000, action),
                    Kind::Sponsored { fee } => sponsored(from, nonce(), *fee, 100_000, action),
                    Kind::Unlock { pick, bogus } => {
                        let (target_height, index) = if *bogus && pick % 2 == 0 {
                            // This block's own height, or a height that does not exist yet
                            (height + u64::try_from(*pick).unwrap(), 0)
                        } else if *bogus {
                            // Each height accepts only one block
                            (height.saturating_sub(1), 1 + u32::try_from(*pick).unwrap())
                        } else {
                            segment.get(pick % segment.len().max(1))?.position()
                        };
                        unlock(target_height, index)
                    }
                })
            })
            .collect();
        let producer = users[i % USERS].account;
        let env = BlockEnv {
            height,
            btc_anchor: BtcAnchor {
                height: 100,
                hash: [1; 32],
            },
            producer,
            timestamp: 1_700_000_000 + height,
            rules: DEVNET.rules(height),
            unlockable: segment,
        };
        let summary = match exec.execute_block(Arc::new(view), Arc::new(SeqBtc), &env, &entries) {
            Ok(out) => {
                db.commit(height, &out.writes).unwrap();
                let root = db.root(height).unwrap();
                main.push(UnlockItem {
                    height,
                    index: 0,
                    hash: sha256(&[root.as_slice(), &height.to_le_bytes()].concat()),
                    producer,
                    daa_score: height,
                    weight: 1,
                    rewarded: true,
                });
                format!("{:?}", out.receipts)
            }
            Err(e) => format!("invalid: {e}"),
        };
        let root = db.root(db.latest_version().unwrap()).unwrap();
        results.push((summary, root));
    }
    results
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn random_entry_sequences_execute_identically(blocks in blocks()) {
        let first = run(Vm::with_parallel_compilation(&DEVNET, true).unwrap(), &blocks);
        let second = run(Vm::with_parallel_compilation(&DEVNET, false).unwrap(), &blocks);
        prop_assert_eq!(first, second);
    }
}
