//! Pruning (D-097): a DAG pruned at a block `P` on the selected chain of a block `F`, at least
//! [`GhostdagParams::min_pruning_depth`] blue score below it, after everything in the past of `F` has been inserted,
//! computes exactly what the unpruned DAG computes for every block it accepts, never accepts a block the unpruned DAG
//! rejects, and accepts every block that a later chain through `F` needs (the blocks in its past that are not in the
//! past of `F`). Checked on random DAGs made by honest miners with network delay, with and without an attacker that
//! extends low-work side chains and tries random merges.

use std::collections::BTreeSet;

use ksana_consensus::{
    BlockDag, DagCheckpoint, GhostdagData, GhostdagError, GhostdagParams, Hash, U256,
};
use ksana_primitives::hash::sha256;

/// Deterministic pseudo-random numbers (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn id(seed: u64, i: u64) -> Hash {
    sha256(&[seed.to_le_bytes(), i.to_le_bytes()].concat())
}

fn params() -> GhostdagParams {
    GhostdagParams {
        k: 3,
        max_block_parents: 6,
        max_mergeset_size: 64,
        merge_depth: 20,
    }
}

/// One block offered to the DAG, in creation order.
struct Offered {
    hash: Hash,
    parents: Vec<Hash>,
    work: U256,
    /// The unpruned DAG's GHOSTDAG data; `None` when it rejected the block.
    data: Option<GhostdagData>,
}

/// A network of honest miners (and optionally an attacker): every new block sees the blocks created up to `max_delay`
/// steps before it. An honest miner takes the heaviest visible tip and adds the other visible tips, heaviest first,
/// while the unpruned DAG still accepts the block (like a block template). The attacker extends low-work side chains
/// from random visible blocks and offers blocks merging random visible blocks.
fn simulate(seed: u64, n: u64, max_delay: u64, attacker: bool) -> (BlockDag<()>, Vec<Offered>) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let genesis = id(seed, 0);
    let mut full = BlockDag::new(params(), genesis, U256::ONE, ());
    let mut offered: Vec<Offered> = Vec::new();
    // accepted blocks in creation order, with their parents
    let mut accepted: Vec<(Hash, Vec<Hash>)> = vec![(genesis, Vec::new())];
    let mut side_tip: Option<Hash> = None;
    for i in 1..=n {
        let delay = rng.below(max_delay + 1) as usize;
        let visible = &accepted[..accepted.len().saturating_sub(delay).max(1)];
        let hash = id(seed, i);
        let roll = rng.below(100);
        let (parents, work) = if attacker && roll < 8 {
            // extend a low-work side chain, sometimes starting a new one from a random visible block
            let parent = match side_tip {
                Some(tip) if rng.below(10) != 0 && full.contains(&tip) => tip,
                _ => visible[rng.below(visible.len() as u64) as usize].0,
            };
            (vec![parent], U256::ONE)
        } else if attacker && roll < 16 {
            // merge random visible blocks (often invalid)
            let count = 2 + rng.below(3) as usize;
            let mut parents = BTreeSet::new();
            for _ in 0..count {
                parents.insert(visible[rng.below(visible.len() as u64) as usize].0);
            }
            (
                parents.into_iter().collect(),
                U256::from_u64(5 + rng.below(11)),
            )
        } else {
            let mut has_child = BTreeSet::new();
            for (_, parents) in visible {
                has_child.extend(parents.iter().copied());
            }
            let mut tips: Vec<(U256, Hash)> = visible
                .iter()
                .filter(|(h, _)| !has_child.contains(h))
                .map(|(h, _)| full.sort_key(h).unwrap())
                .collect();
            tips.sort_unstable_by(|a, b| b.cmp(a));
            let mut parents = vec![tips[0].1];
            for (_, tip) in tips.iter().skip(1) {
                if parents.len() == params().max_block_parents {
                    break;
                }
                parents.push(*tip);
                if full.compute(&parents, 1).is_err() {
                    parents.pop();
                }
            }
            (parents, U256::from_u64(5 + rng.below(11)))
        };
        let data = full
            .insert(hash, parents.clone(), work, 1, 0, ())
            .ok()
            .cloned();
        if data.is_some() {
            if parents.len() == 1 && work == U256::ONE {
                side_tip = Some(hash);
            }
            accepted.push((hash, parents.clone()));
        }
        offered.push(Offered {
            hash,
            parents,
            work,
            data,
        });
    }
    (full, offered)
}

/// The block on the selected chain of `from` with blue score at most `score` (the first one going down).
fn chain_block_at_most(dag: &BlockDag<()>, from: &Hash, score: u64) -> Hash {
    let mut current = *from;
    loop {
        let data = dag.data(&current).unwrap();
        match data.selected_parent {
            Some(sp) if data.blue_score > score => current = sp,
            _ => return current,
        }
    }
}

/// A checkpoint of `dag` taken through its Borsh encoding and restored.
fn round_trip(dag: &BlockDag<()>) -> BlockDag<()> {
    let bytes = borsh::to_vec(&dag.checkpoint().unwrap()).unwrap();
    let checkpoint: DagCheckpoint<()> = borsh::from_slice(&bytes).unwrap();
    BlockDag::restore(params(), checkpoint).unwrap()
}

/// Replays the offered blocks into a second DAG, pruning it once `F` (a block on the final best chain, `f_share` of the
/// way up it) has been inserted, and compares with the unpruned DAG; a third DAG restored from a checkpoint of the
/// pruned one right after pruning must then process the remaining blocks exactly like it. Returns (blocks pruned,
/// blocks rejected as needing a pruned block).
fn check_pruning(seed: u64, n: u64, max_delay: u64, attacker: bool) -> (usize, usize) {
    let (full, offered) = simulate(seed, n, max_delay, attacker);
    let ctx = format!("seed {seed} delay {max_delay} attacker {attacker}");
    let tip = full.selected_chain_tips_desc(None).next().unwrap().1;
    let tip_score = full.data(&tip).unwrap().blue_score;
    let f = chain_block_at_most(&full, &tip, tip_score * 3 / 5);
    let f_score = full.data(&f).unwrap().blue_score;
    let depth = params().min_pruning_depth();
    assert!(f_score > depth, "{ctx}: chain too short");
    let p = chain_block_at_most(&full, &f, f_score - depth);
    let f_index = offered.iter().position(|o| o.hash == f).unwrap();

    let needed = |hash: &Hash| {
        full.in_closed_past(hash, &tip).unwrap() && !full.in_closed_past(hash, &f).unwrap()
    };
    let mut pruned_dag = BlockDag::new(params(), *full.genesis(), U256::ONE, ());
    let mut restored: Option<BlockDag<()>> = None;
    let mut dropped = 0;
    let mut unprocessable = 0;
    for (index, o) in offered.iter().enumerate() {
        let got = pruned_dag
            .insert(o.hash, o.parents.clone(), o.work, 1, 0, ())
            .cloned();
        if let Some(restored) = restored.as_mut() {
            let again = restored
                .insert(o.hash, o.parents.clone(), o.work, 1, 0, ())
                .cloned();
            assert_eq!(again, got, "{ctx}: block {index} after restoring");
        }
        match (&got, &o.data) {
            (Ok(got), Some(want)) => assert_eq!(got, want, "{ctx}: block {index}"),
            (Ok(_), None) => {
                panic!("{ctx}: block {index} accepted after pruning, rejected by the full DAG")
            }
            (Err(e), Some(_)) => {
                assert!(
                    index > f_index,
                    "{ctx}: block {index} rejected before pruning: {e}"
                );
                assert!(
                    !needed(&o.hash),
                    "{ctx}: needed block {index} rejected after pruning: {e}"
                );
                assert!(
                    matches!(
                        e,
                        GhostdagError::Pruned(_) | GhostdagError::UnknownParent(_)
                    ),
                    "{ctx}: block {index}: {e}"
                );
                unprocessable += 1;
            }
            (Err(_), None) => {}
        }
        if index == f_index {
            dropped = pruned_dag.prune(&p).unwrap();
            assert_eq!(pruned_dag.pruning_point(), Some(&p));
            let copy = round_trip(&pruned_dag);
            assert!(copy == pruned_dag, "{ctx}: restored DAG differs");
            restored = Some(copy);
        }
    }
    assert!(dropped > 0, "{ctx}");
    assert!(
        restored.as_ref() == Some(&pruned_dag),
        "{ctx}: restored DAG diverged"
    );
    // every needed block is kept; ancestor queries among the kept blocks agree
    let kept: Vec<Hash> = pruned_dag.iter().map(|(h, _)| *h).collect();
    for o in &offered {
        if o.data.is_some() && needed(&o.hash) {
            assert!(pruned_dag.contains(&o.hash), "{ctx}");
        }
    }
    let mut rng = Rng(seed | 1);
    for _ in 0..2000 {
        let x = kept[rng.below(kept.len() as u64) as usize];
        let y = kept[rng.below(kept.len() as u64) as usize];
        assert_eq!(
            pruned_dag.in_past(&x, &y).unwrap(),
            full.in_past(&x, &y).unwrap(),
            "{ctx}"
        );
    }
    // the kept blocks are exactly P and its future
    for (hash, _) in full.iter() {
        let in_future = *hash == p || full.in_past(&p, hash).unwrap();
        if pruned_dag.contains(hash) {
            assert!(in_future, "{ctx}");
        }
    }
    (dropped, unprocessable)
}

#[test]
fn a_pruned_dag_matches_the_full_dag() {
    let mut dropped = 0;
    for seed in 1..=12u64 {
        for max_delay in [0, 2, 5] {
            dropped += check_pruning(seed, 600, max_delay, false).0;
        }
    }
    assert!(dropped > 0);
}

#[test]
fn a_pruned_dag_matches_the_full_dag_with_an_attacker() {
    let (mut dropped, mut unprocessable) = (0, 0);
    for seed in 100..=124u64 {
        for max_delay in [1, 4] {
            let (d, u) = check_pruning(seed, 600, max_delay, true);
            dropped += d;
            unprocessable += u;
        }
    }
    assert!(dropped > 0);
    // the attacker's side chains forked below the pruning point cannot be processed any more
    assert!(unprocessable > 0);
}

/// A block whose parent has been pruned cannot be processed (it is reported, not judged invalid), nor can a block
/// whose selected parent is kept but too close to the pruning point; ancestor queries from a kept block into the pruned
/// part answer `false`, and the genesis block is still in every block's past.
#[test]
fn pruned_blocks_are_reported() {
    let seed = 7;
    let genesis = id(seed, 0);
    let mut dag = BlockDag::new(params(), genesis, U256::ONE, ());
    let mut chain = vec![genesis];
    for i in 1..=200u64 {
        dag.insert(
            id(seed, i),
            vec![chain[chain.len() - 1]],
            U256::ONE,
            1,
            0,
            (),
        )
        .unwrap();
        chain.push(id(seed, i));
    }
    let side = id(seed, 1000);
    dag.insert(side, vec![chain[10]], U256::ONE, 1, 0, ())
        .unwrap();
    assert_eq!(dag.prune(&chain[100]).unwrap(), 101);
    assert_eq!(dag.len(), 101);
    assert!(!dag.contains(&side));
    assert!(!dag.contains(&genesis));
    assert_eq!(dag.tips(), &BTreeSet::from([chain[200]]));
    assert!(dag.in_past(&chain[100], &chain[200]).unwrap());
    assert!(!dag.in_past(&chain[200], &chain[150]).unwrap());
    assert!(dag.in_past(&genesis, &chain[200]).unwrap());
    // a child of a pruned block
    assert_eq!(
        dag.compute(&[side], 1),
        Err(GhostdagError::UnknownParent(side))
    );
    // the selected chain must be kept down to 2 × (merge depth + K) = 46 blue score below the selected parent
    assert_eq!(
        dag.compute(&[chain[140]], 1),
        Err(GhostdagError::Pruned(chain[99]))
    );
    assert!(dag.compute(&[chain[146]], 1).is_ok());
    // a block on a kept block deep below the merge-depth root cannot be merged by the tip
    let low = id(seed, 1001);
    dag.insert(low, vec![chain[150]], U256::ONE, 1, 0, ())
        .unwrap();
    assert_eq!(
        dag.compute(&[chain[200], low], 1),
        Err(GhostdagError::MergeDepthRoot(low))
    );
    let sorted: Vec<(U256, Hash)> = dag.selected_chain_tips_desc(None).copied().collect();
    let mut by_key = sorted.clone();
    by_key.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(sorted, by_key);
    assert_eq!(sorted.len(), 2);
}

/// A checkpoint of an unpruned DAG restores the same DAG, and inconsistent checkpoints are refused.
#[test]
fn checkpoints_restore_the_same_dag() {
    let (full, _) = simulate(3, 300, 2, true);
    let copy = round_trip(&full);
    assert!(copy == full);
    assert_eq!(copy.pruning_point(), None);

    let mut checkpoint = full.checkpoint().unwrap();
    // a block missing: its parents' children are not all in the checkpoint
    let mut broken = checkpoint.clone();
    broken.blocks.pop();
    assert!(matches!(
        BlockDag::restore(params(), broken),
        Err(GhostdagError::Corrupt(_))
    ));
    // the root (the genesis block) missing
    checkpoint.blocks.retain(|b| b.hash != *full.genesis());
    assert!(matches!(
        BlockDag::restore(params(), checkpoint),
        Err(GhostdagError::Corrupt(_))
    ));
}
