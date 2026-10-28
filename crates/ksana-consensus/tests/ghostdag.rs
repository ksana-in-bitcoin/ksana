//! GHOSTDAG (D-063): compared block by block against a brute-force reference implementation that computes directly
//! with sets per the definitions; the k-cluster property; ancestor queries; merge limits.

use std::collections::{BTreeMap, BTreeSet};

use ksana_consensus::{BlockDag, GhostdagError, GhostdagParams, Hash, U256};
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

fn id(i: u64) -> Hash {
    sha256(&i.to_le_bytes())
}

fn params(k: u16) -> GhostdagParams {
    GhostdagParams {
        k,
        max_block_parents: 10,
        max_mergeset_size: 10_000,
        merge_depth: 1_000_000,
    }
}

/// The result for one block in the reference implementation.
#[derive(Clone, Debug)]
struct RefBlock {
    work: U256,
    past: BTreeSet<Hash>,
    selected_parent: Option<Hash>,
    blue_score: u64,
    blue_work: U256,
    mergeset_blues: Vec<Hash>,
    mergeset_reds: Vec<Hash>,
    ordered: Vec<Hash>,
    /// The blue blocks in this block's past (per this block's coloring).
    blue_set: BTreeSet<Hash>,
}

/// Computes per the definitions: past and anticone are both computed directly with sets.
struct Reference {
    k: usize,
    blocks: BTreeMap<Hash, RefBlock>,
}

impl Reference {
    fn new(k: u16, genesis: Hash, work: U256) -> Self {
        let g = RefBlock {
            work,
            past: BTreeSet::new(),
            selected_parent: None,
            blue_score: 0,
            blue_work: U256::ZERO,
            mergeset_blues: Vec::new(),
            mergeset_reds: Vec::new(),
            ordered: Vec::new(),
            blue_set: BTreeSet::new(),
        };
        Reference {
            k: usize::from(k),
            blocks: BTreeMap::from([(genesis, g)]),
        }
    }

    /// The sort key `(blue_work, hash)`: the merge set's order.
    fn key(&self, h: &Hash) -> (U256, Hash) {
        (self.blocks[h].blue_work, *h)
    }

    /// The fork-choice key `(blue_work + work, hash)` (D-100): the selected parent's.
    fn chain_key(&self, h: &Hash) -> (U256, Hash) {
        let block = &self.blocks[h];
        (block.blue_work.checked_add(block.work).unwrap(), *h)
    }

    fn in_anticone(&self, a: &Hash, b: &Hash) -> bool {
        a != b && !self.blocks[a].past.contains(b) && !self.blocks[b].past.contains(a)
    }

    fn insert(&mut self, hash: Hash, parents: &[Hash], work: U256) {
        let mut past = BTreeSet::new();
        for p in parents {
            past.insert(*p);
            past.extend(self.blocks[p].past.iter().copied());
        }
        let sp = *parents.iter().max_by_key(|p| self.chain_key(p)).unwrap();
        let sp_block = self.blocks[&sp].clone();
        let mut ordered: Vec<Hash> = past
            .iter()
            .filter(|x| **x != sp && !sp_block.past.contains(*x))
            .copied()
            .collect();
        ordered.sort_by_key(|x| self.key(x));

        let mut blues = sp_block.blue_set.clone();
        blues.insert(sp);
        let mut mergeset_blues = vec![sp];
        let mut mergeset_reds = Vec::new();
        for c in &ordered {
            let anticone: Vec<Hash> = blues
                .iter()
                .filter(|b| !self.blocks[c].past.contains(*b))
                .copied()
                .collect();
            for b in &anticone {
                // none of the current blue blocks is in the candidate's future
                assert!(!self.blocks[b].past.contains(c));
            }
            let blue = mergeset_blues.len() < self.k + 1
                && anticone.len() <= self.k
                && anticone
                    .iter()
                    .all(|b| blues.iter().filter(|x| self.in_anticone(b, x)).count() < self.k);
            if blue {
                mergeset_blues.push(*c);
                blues.insert(*c);
            } else {
                mergeset_reds.push(*c);
            }
        }
        let blue_score = sp_block.blue_score + mergeset_blues.len() as u64;
        let mut blue_work = sp_block.blue_work;
        for b in &mergeset_blues {
            blue_work = blue_work.checked_add(self.blocks[b].work).unwrap();
        }
        self.blocks.insert(
            hash,
            RefBlock {
                work,
                past,
                selected_parent: Some(sp),
                blue_score,
                blue_work,
                mergeset_blues,
                mergeset_reds,
                ordered,
                blue_set: blues,
            },
        );
    }
}

/// Random DAG: block i sees all blocks at least `delay` (random in 0..=max_delay) blocks older than itself and takes
/// the tips among them as parents (a random subset when there are more than `max_parents`). Returns (hash, parents,
/// work) in insertion order.
fn random_dag(
    seed: u64,
    n: u64,
    max_delay: u64,
    max_parents: usize,
) -> (Hash, Vec<(Hash, Vec<Hash>, U256)>) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let genesis = id(0);
    let mut order = vec![genesis];
    let mut parents_of: BTreeMap<Hash, Vec<Hash>> = BTreeMap::from([(genesis, vec![])]);
    let mut out = Vec::new();
    for i in 1..=n {
        let delay = rng.below(max_delay + 1);
        let visible = (i as usize).saturating_sub(delay as usize).max(1);
        let seen: BTreeSet<Hash> = order.iter().take(visible).copied().collect();
        let mut has_child = BTreeSet::new();
        for h in &seen {
            for p in &parents_of[h] {
                has_child.insert(*p);
            }
        }
        let mut tips: Vec<Hash> = seen.difference(&has_child).copied().collect();
        while tips.len() > max_parents {
            let drop = rng.below(tips.len() as u64) as usize;
            tips.remove(drop);
        }
        let hash = id(i);
        let work = U256::from_u64(1 + rng.below(3));
        parents_of.insert(hash, tips.clone());
        order.push(hash);
        out.push((hash, tips, work));
    }
    (genesis, out)
}

fn check_against_reference(seed: u64, n: u64, max_delay: u64, k: u16) -> (usize, usize) {
    let (genesis, blocks) = random_dag(seed, n, max_delay, 10);
    let mut dag = BlockDag::new(params(k), genesis, U256::ONE, ());
    let mut reference = Reference::new(k, genesis, U256::ONE);
    let (mut reds, mut merges) = (0, 0);
    for (hash, parents, work) in &blocks {
        let got = dag
            .insert(*hash, parents.clone(), *work, 1, 0, ())
            .unwrap()
            .clone();
        reference.insert(*hash, parents, *work);
        let want = &reference.blocks[hash];
        let ctx = format!("seed {seed} block {}", hex::encode(&hash[..4]));
        assert_eq!(got.selected_parent, want.selected_parent, "{ctx}");
        assert_eq!(got.mergeset_ordered, want.ordered, "{ctx}");
        assert_eq!(got.mergeset_blues, want.mergeset_blues, "{ctx}");
        assert_eq!(got.mergeset_reds, want.mergeset_reds, "{ctx}");
        assert_eq!(got.blue_score, want.blue_score, "{ctx}");
        assert_eq!(got.blue_work, want.blue_work, "{ctx}");
        assert_eq!(got.daa_score, want.past.len() as u64, "{ctx}");
        assert_eq!(got.mergeset_len(), want.ordered.len() + 1, "{ctx}");
        for x in want.ordered.iter().chain(want.selected_parent.iter()) {
            assert!(got.mergeset_contains(x), "{ctx}");
        }
        // the blue set is a k-cluster, and the queried blue anticone size equals the one counted with sets
        for b in &want.blue_set {
            let anticone = want
                .blue_set
                .iter()
                .filter(|x| reference.in_anticone(b, x))
                .count();
            assert!(
                anticone <= usize::from(k),
                "{ctx}: blue anticone {anticone} > K"
            );
            assert_eq!(
                usize::from(dag.blue_anticone_size_in(b, hash).unwrap()),
                anticone,
                "{ctx}"
            );
        }
        reds += want.mergeset_reds.len();
        merges += want.ordered.len();
    }
    // ancestor queries agree with the sets (sampled)
    let hashes: Vec<Hash> = reference.blocks.keys().copied().collect();
    let mut rng = Rng(seed | 1);
    for _ in 0..2000 {
        let x = hashes[rng.below(hashes.len() as u64) as usize];
        let y = hashes[rng.below(hashes.len() as u64) as usize];
        assert_eq!(
            dag.in_past(&x, &y).unwrap(),
            reference.blocks[&y].past.contains(&x),
            "seed {seed}"
        );
    }
    // tips = blocks without children
    let mut with_children = BTreeSet::new();
    for (_, parents, _) in &blocks {
        with_children.extend(parents.iter().copied());
    }
    let tips: BTreeSet<Hash> = hashes
        .iter()
        .filter(|h| !with_children.contains(*h))
        .copied()
        .collect();
    assert_eq!(dag.tips(), &tips);
    (reds, merges)
}

#[test]
fn matches_the_reference_on_random_dags() {
    let mut total_reds = 0;
    let mut total_merged = 0;
    for seed in 0..24 {
        let k = [0u16, 1, 2, 3, 8][seed as usize % 5];
        let max_delay = [0u64, 1, 3, 6, 10][(seed as usize / 5) % 5];
        let (reds, merged) = check_against_reference(seed, 160, max_delay, k);
        total_reds += reds;
        total_merged += merged;
    }
    // the scenarios do cover parallel blocks and red blocks
    assert!(total_merged > 500, "{total_merged}");
    assert!(total_reds > 100, "{total_reds}");
}

/// The queries used for sync (D-063): iteration by sort key ascending (topological order), going back along the
/// selected chain by blue score, and the blocks in `past(x) ∪ {x}` with key above the floor, agree with the reference
/// implementation's sets.
#[test]
fn sync_queries_match_the_reference() {
    for seed in 0..8 {
        let (genesis, blocks) = random_dag(100 + seed, 120, [0, 2, 5, 9][seed as usize % 4], 10);
        let mut dag = BlockDag::new(params(3), genesis, U256::ONE, ());
        let mut reference = Reference::new(3, genesis, U256::ONE);
        for (hash, parents, work) in &blocks {
            dag.insert(*hash, parents.clone(), *work, 1, 0, ()).unwrap();
            reference.insert(*hash, parents, *work);
        }
        let key = |h: &Hash| dag.sort_key(h).unwrap();
        // all blocks by key ascending, parents always before children
        let all: Vec<Hash> = dag.after(None).copied().collect();
        assert_eq!(all.len(), dag.len());
        assert!(all.windows(2).all(|w| key(&w[0]) < key(&w[1])));
        let position: BTreeMap<Hash, usize> =
            all.iter().enumerate().map(|(i, h)| (*h, i)).collect();
        for (hash, parents, _) in &blocks {
            for p in parents {
                assert!(position[p] < position[hash]);
            }
        }
        let mut rng = Rng(seed | 1);
        for _ in 0..40 {
            let x = all[rng.below(all.len() as u64) as usize];
            let floor_block = all[rng.below(all.len() as u64) as usize];
            let floor = key(&floor_block);
            // the blocks with key above the floor
            let above: Vec<Hash> = dag.after(Some(&floor)).copied().collect();
            let want: Vec<Hash> = all.iter().filter(|h| key(h) > floor).copied().collect();
            assert_eq!(above, want);
            // the blocks in past(x) ∪ {x} with key above the floor
            let got = dag.closed_past_above(&x, &floor).unwrap();
            let want: BTreeSet<Hash> = reference.blocks[&x]
                .past
                .iter()
                .chain([&x])
                .filter(|h| key(h) > floor)
                .copied()
                .collect();
            assert_eq!(got, want, "seed {seed}");
            // going back depth in blue score along the selected chain: the result is on x's selected chain, its blue
            // score does not exceed the target, and its child on the chain exceeds the target
            let depth = rng.below(20);
            let (work, floor_hash) = dag.chain_floor(&x, depth).unwrap();
            assert_eq!(work, dag.data(&floor_hash).unwrap().blue_work);
            let target = dag.data(&x).unwrap().blue_score.saturating_sub(depth);
            let mut chain = vec![x];
            while let Some(sp) = dag.data(chain.last().unwrap()).unwrap().selected_parent {
                chain.push(sp);
            }
            let at = chain
                .iter()
                .position(|h| *h == floor_hash)
                .expect("on the selected chain");
            assert!(dag.data(&floor_hash).unwrap().blue_score <= target || floor_hash == genesis);
            if at > 0 {
                assert!(dag.data(&chain[at - 1]).unwrap().blue_score > target);
            }
        }
    }
}

#[test]
fn a_chain_has_scores_equal_to_heights() {
    let genesis = id(0);
    let mut dag = BlockDag::new(params(8), genesis, U256::from_u64(5), ());
    let mut parent = genesis;
    for i in 1..=50u64 {
        let data = dag
            .insert(id(i), vec![parent], U256::from_u64(5), 1, 0, ())
            .unwrap();
        assert_eq!(data.selected_parent, Some(parent));
        assert_eq!(data.blue_score, i);
        assert_eq!(data.daa_score, i);
        assert_eq!(data.blue_work, U256::from_u64(5 * i));
        assert!(data.mergeset_ordered.is_empty());
        assert_eq!(data.mergeset_blues, vec![parent]);
        parent = id(i);
    }
    assert!(dag.in_past(&genesis, &id(50)).unwrap());
    assert!(dag.in_past(&id(1), &id(50)).unwrap());
    assert!(!dag.in_past(&id(50), &id(1)).unwrap());
    assert!(!dag.in_past(&id(7), &id(7)).unwrap());
    assert!(dag.in_closed_past(&id(7), &id(7)).unwrap());
}

/// K = 1, three parallel blocks merged by the same block: the selected parent and the parallel block with the smaller
/// hash are blue, the third is red.
#[test]
fn three_siblings_with_k_one() {
    let genesis = id(0);
    let mut dag = BlockDag::new(params(1), genesis, U256::ONE, ());
    let mut siblings = vec![id(1), id(2), id(3)];
    for s in &siblings {
        dag.insert(*s, vec![genesis], U256::ONE, 1, 0, ()).unwrap();
    }
    siblings.sort();
    let [low, mid, high] = [siblings[0], siblings[1], siblings[2]];
    let data = dag
        .insert(id(4), vec![low, mid, high], U256::ONE, 1, 0, ())
        .unwrap()
        .clone();
    // equal chain work, the greatest hash is the selected parent; the rest by hash ascending
    assert_eq!(data.selected_parent, Some(high));
    assert_eq!(data.mergeset_ordered, vec![low, mid]);
    assert_eq!(data.mergeset_blues, vec![high, low]);
    assert_eq!(data.mergeset_reds, vec![mid]);
    assert_eq!(data.blue_score, 3);
    assert_eq!(data.daa_score, 4);
    assert_eq!(dag.tips(), &BTreeSet::from([id(4)]));
}

#[test]
fn parent_rules_are_enforced() {
    let genesis = id(0);
    let mut dag = BlockDag::new(params(8), genesis, U256::ONE, ());
    dag.insert(id(1), vec![genesis], U256::ONE, 1, 0, ())
        .unwrap();
    dag.insert(id(2), vec![id(1)], U256::ONE, 1, 0, ()).unwrap();
    dag.insert(id(3), vec![genesis], U256::ONE, 1, 0, ())
        .unwrap();
    assert_eq!(dag.compute(&[], 1), Err(GhostdagError::NoParents));
    assert_eq!(
        dag.compute(&[id(2), id(2)], 1),
        Err(GhostdagError::DuplicateParent)
    );
    assert_eq!(
        dag.compute(&[id(2), id(99)], 1),
        Err(GhostdagError::UnknownParent(id(99)))
    );
    // id(1) is in the past of id(2) (regardless of order)
    for parents in [[id(2), id(1)], [id(1), id(2)]] {
        assert_eq!(
            dag.compute(&parents, 1),
            Err(GhostdagError::ParentInPast(id(1)))
        );
    }
    // the genesis block is in the past of every block
    assert_eq!(
        dag.compute(&[id(3), genesis], 1),
        Err(GhostdagError::ParentInPast(genesis))
    );
    // a parent is in the past of another parent, but neither is the selected parent: id(1) is in the past of id(5)
    dag.insert(id(5), vec![id(1)], U256::ONE, 1, 0, ()).unwrap();
    dag.insert(id(6), vec![id(2), id(3)], U256::ONE, 1, 0, ())
        .unwrap();
    let r = dag.compute(&[id(6), id(5), id(1)], 1);
    assert_eq!(r, Err(GhostdagError::ParentInPast(id(1))));
    assert!(dag.compute(&[id(6), id(5)], 1).is_ok());
    // an ancestor relation between two non-selected parents (not in the selected parent's past), discovered by the
    // merge set traversal
    let mut heavy = genesis;
    for i in 10..15u64 {
        dag.insert(id(i), vec![heavy], U256::from_u64(10), 1, 0, ())
            .unwrap();
        heavy = id(i);
    }
    dag.insert(id(20), vec![genesis], U256::ONE, 1, 0, ())
        .unwrap();
    dag.insert(id(21), vec![id(20)], U256::ONE, 1, 0, ())
        .unwrap();
    assert_eq!(
        dag.compute(&[heavy, id(21), id(20)], 1),
        Err(GhostdagError::ParentInPast(id(20)))
    );
    assert_eq!(
        dag.compute(&[heavy, id(21)], 1).unwrap().selected_parent,
        Some(heavy)
    );
    // the maximum number of parents
    let mut wide = BlockDag::new(
        GhostdagParams {
            max_block_parents: 3,
            ..params(8)
        },
        genesis,
        U256::ONE,
        (),
    );
    for i in 1..=4 {
        wide.insert(id(i), vec![genesis], U256::ONE, 1, 0, ())
            .unwrap();
    }
    assert_eq!(
        wide.compute(&[id(1), id(2), id(3), id(4)], 1),
        Err(GhostdagError::TooManyParents { count: 4, max: 3 })
    );
    assert!(wide.compute(&[id(1), id(2), id(3)], 1).is_ok());
    assert_eq!(
        wide.insert(id(1), vec![genesis], U256::ONE, 1, 0, ()),
        Err(GhostdagError::AlreadyKnown)
    );
}

/// Two branches of 10 blocks each: the merging block's merge set has 11 blocks (including the selected parent).
#[test]
fn mergeset_size_limit() {
    let genesis = id(0);
    let build = |max_mergeset_size| {
        let mut dag = BlockDag::new(
            GhostdagParams {
                max_mergeset_size,
                ..params(8)
            },
            genesis,
            U256::ONE,
            (),
        );
        let (mut a, mut b) = (genesis, genesis);
        for i in 1..=10u64 {
            dag.insert(id(100 + i), vec![a], U256::ONE, 1, 0, ())
                .unwrap();
            dag.insert(id(200 + i), vec![b], U256::ONE, 1, 0, ())
                .unwrap();
            a = id(100 + i);
            b = id(200 + i);
        }
        (dag, a, b)
    };
    let (dag, a, b) = build(10);
    assert_eq!(
        dag.compute(&[a, b], 1),
        Err(GhostdagError::MergesetTooLarge { max: 10 })
    );
    let (dag, a, b) = build(11);
    let data = dag.compute(&[a, b], 1).unwrap();
    assert_eq!(data.mergeset_len(), 11);
    assert_eq!(data.daa_score, 21);
}

/// A 30-block main chain and a child of the genesis block (D-097): the child's selected chain (itself, the genesis
/// block) passes through the merge-depth root only while the root is still the genesis block, i.e.
/// `30 − merge_depth ≤ 0`; the blue-score rule alone (`1 + merge_depth ≥ 30`) would already allow it from 29 on.
#[test]
fn merge_depth_limit() {
    let genesis = id(0);
    let build = |merge_depth| {
        let mut dag = BlockDag::new(
            GhostdagParams {
                merge_depth,
                ..params(8)
            },
            genesis,
            U256::ONE,
            (),
        );
        let mut tip = genesis;
        for i in 1..=30u64 {
            dag.insert(id(i), vec![tip], U256::ONE, 1, 0, ()).unwrap();
            tip = id(i);
        }
        dag.insert(id(999), vec![genesis], U256::ONE, 1, 0, ())
            .unwrap();
        (dag, tip)
    };
    for merge_depth in [28, 29] {
        let (dag, tip) = build(merge_depth);
        assert_eq!(
            dag.compute(&[tip, id(999)], 1),
            Err(GhostdagError::MergeDepthRoot(id(999)))
        );
    }
    let (dag, tip) = build(30);
    let data = dag.compute(&[tip, id(999)], 1).unwrap();
    assert_eq!(data.mergeset_ordered, vec![id(999)]);
}

/// The two merge-depth rules (D-063, D-097): the root is the first block on the selected parent's chain whose blue
/// score is at most `blue_score(SP) − merge_depth`; a merged block must have the root on its selected chain, and its
/// own blue score within `merge_depth` of the selected parent's. Here a chain step that adds four blue blocks leaves
/// the root below that bound, so a block on the root can still fail the blue-score rule.
#[test]
fn merge_depth_root_and_blue_score() {
    let genesis = id(0);
    let siblings: Vec<Hash> = (1..=4).map(id).collect();
    // equal blue work: the largest hash is the selected parent of the block merging them
    let a = *siblings.iter().max().unwrap();
    let b = *siblings.iter().min().unwrap();
    let (x, y) = (id(21), id(22));
    let build = |merge_depth| {
        let mut dag = BlockDag::new(
            GhostdagParams {
                merge_depth,
                ..params(8)
            },
            genesis,
            U256::ONE,
            (),
        );
        for sibling in &siblings {
            dag.insert(*sibling, vec![genesis], U256::ONE, 1, 0, ())
                .unwrap();
        }
        // blue score 1 + 4 = 5, then 6 ..= 14 along a plain chain
        dag.insert(id(10), siblings.clone(), U256::ONE, 1, 0, ())
            .unwrap();
        let mut tip = id(10);
        for i in 11..=19u64 {
            dag.insert(id(i), vec![tip], U256::ONE, 1, 0, ()).unwrap();
            tip = id(i);
        }
        // x on a (the root below), y on b (not on the selected chain); both have blue score 2
        dag.insert(x, vec![a], U256::ONE, 1, 0, ()).unwrap();
        dag.insert(y, vec![b], U256::ONE, 1, 0, ()).unwrap();
        (dag, tip)
    };
    let (dag, tip) = build(10);
    assert_eq!(dag.data(&tip).unwrap().blue_score, 14);
    assert_eq!(dag.data(&id(10)).unwrap().selected_parent, Some(a));
    assert_eq!(dag.chain_floor(&tip, 10).unwrap().1, a);
    assert_eq!(dag.compute(&[tip, x], 1), Err(GhostdagError::MergeDepth(x)));
    assert_eq!(
        dag.compute(&[tip, y], 1),
        Err(GhostdagError::MergeDepthRoot(y))
    );
    // 2 + 12 ≥ 14: x can be merged; the root is still a, so y still cannot
    let (dag, tip) = build(12);
    assert_eq!(dag.compute(&[tip, x], 1).unwrap().mergeset_ordered, vec![x]);
    assert_eq!(
        dag.compute(&[tip, y], 1),
        Err(GhostdagError::MergeDepthRoot(y))
    );
    // the root is the genesis block: both can be merged
    let (dag, tip) = build(14);
    assert_eq!(dag.chain_floor(&tip, 14).unwrap().1, genesis);
    assert_eq!(dag.compute(&[tip, x, y], 1).unwrap().mergeset_len(), 3);
}

/// D-097: a side chain of low-work blocks forked from the genesis block, merged by the main chain one block at a time,
/// keeps its blue score within the merge depth of the main chain's (the blue-score rule alone would let it be merged
/// forever, each merge walking the main chain back to the fork). Once the main chain is `merge_depth` deep its
/// selected chain no longer passes through the merge-depth root, and it cannot be merged any more.
#[test]
fn a_low_work_side_chain_stops_being_mergeable() {
    let genesis = id(0);
    let merge_depth = 20;
    let mut dag = BlockDag::new(
        GhostdagParams {
            merge_depth,
            ..params(8)
        },
        genesis,
        U256::ONE,
        (),
    );
    // two main-chain blocks first, so that the main chain is the heavier side from the first merge on
    let heavy = U256::from_u64(1000);
    dag.insert(id(1), vec![genesis], heavy, 1, 0, ()).unwrap();
    dag.insert(id(2), vec![id(1)], heavy, 1, 0, ()).unwrap();
    let (mut main, mut side) = (id(2), genesis);
    let mut merged = Vec::new();
    for i in 3..=60u64 {
        let next = id(1000 + i);
        dag.insert(next, vec![side], U256::ONE, 1, 0, ()).unwrap();
        side = next;
        let sp_score = dag.data(&main).unwrap().blue_score;
        let side_score = dag.data(&side).unwrap().blue_score;
        match dag.insert(id(i), vec![main, side], heavy, 1, 0, ()) {
            Ok(_) => merged.push(sp_score),
            Err(e) => {
                assert_eq!(e, GhostdagError::MergeDepthRoot(side));
                assert!(sp_score > merge_depth);
                assert!(side_score + merge_depth >= sp_score);
                dag.insert(id(i), vec![main], heavy, 1, 0, ()).unwrap();
            }
        }
        main = id(i);
    }
    assert!(!merged.is_empty());
    assert!(merged.iter().all(|score| *score <= merge_depth));
}

/// D-084: the DAA score accumulates weights — the selected parent's score, plus the weights of the blocks in the
/// merge set (other than the selected parent; blue and red alike), plus this block's weight; blue score and blue work
/// are unaffected by the weight; `compute` and `insert` agree; overflow is a data inconsistency.
#[test]
fn daa_score_adds_the_weights_of_the_block_and_its_mergeset() {
    let genesis = id(0);
    for k in [0u16, 3] {
        let mut dag = BlockDag::new(params(k), genesis, U256::ONE, ());
        assert_eq!(dag.get(&genesis).unwrap().weight, 1);
        assert_eq!(dag.get(&genesis).unwrap().streak, 0);
        // chain: a1 (empty block, weight 12) → a2 (non-empty block, weight 1)
        let a1 = dag
            .insert(id(1), vec![genesis], U256::ONE, 12, 1, ())
            .unwrap()
            .clone();
        assert_eq!((a1.daa_score, a1.blue_score), (12, 1));
        let a2 = dag
            .insert(id(2), vec![id(1)], U256::ONE, 1, 0, ())
            .unwrap()
            .clone();
        assert_eq!((a2.daa_score, a2.blue_score), (13, 2));
        // parallel block b (parent the genesis block, weight 12)
        let b = dag
            .insert(id(3), vec![genesis], U256::ONE, 12, 1, ())
            .unwrap()
            .clone();
        assert_eq!(b.daa_score, 12);
        // c merges a2 and b: selected parent a2 (higher chain work), D(c) = D(a2) + w(b) + w(c)
        let light = dag.compute(&[id(2), id(3)], 1).unwrap();
        assert_eq!(light.selected_parent, Some(id(2)));
        assert_eq!(light.mergeset_ordered, vec![id(3)]);
        assert_eq!(light.daa_score, 13 + 12 + 1);
        let heavy = dag.compute(&[id(2), id(3)], 12).unwrap();
        assert_eq!(heavy.daa_score, 13 + 12 + 12);
        // at k = 0, b is red (parallel to a1 and a2), at k = 3 it is blue: the weight counts toward the DAA score
        // either way, blue score and blue work do not include weights
        assert_eq!(light.blue_score, if k == 0 { 3 } else { 4 });
        assert_eq!(heavy.blue_score, light.blue_score);
        assert_eq!(heavy.blue_work, light.blue_work);
        let inserted = dag
            .insert(id(4), vec![id(2), id(3)], U256::ONE, 12, 1, ())
            .unwrap()
            .clone();
        assert_eq!(inserted, heavy);
        assert_eq!(dag.get(&id(4)).unwrap().weight, 12);
        assert_eq!(dag.weight(&id(4)), Some(12));
        assert_eq!(dag.streak(&id(4)), Some(1));
    }
    // overflow: adding a weight after a block with weight u64::MAX
    let mut dag = BlockDag::new(params(3), genesis, U256::ONE, ());
    dag.insert(id(1), vec![genesis], U256::ONE, u64::MAX, 1, ())
        .unwrap();
    assert!(matches!(
        dag.compute(&[id(1)], 1),
        Err(GhostdagError::Corrupt(_))
    ));
}

/// D-090: the empty-block streak passes along the selected parent — `parent_streak` gives the streak of the new
/// block's selected parent, regardless of the order of the parents; a non-empty block resets the streak to zero; an
/// empty block that is only merged (not the selected parent) does not affect the streak; the genesis block's streak is
/// 0; no parents or an unknown parent is an error.
#[test]
fn empty_block_streaks_follow_the_selected_parent() {
    let genesis = id(0);
    let mut dag = BlockDag::new(params(3), genesis, U256::ONE, ());
    assert_eq!(dag.streak(&genesis), Some(0));
    assert_eq!(dag.weight(&genesis), Some(1));
    assert_eq!(dag.parent_streak(&[genesis]), Ok(0));
    // chain: e1 (empty block, streak 1, weight 12) → e2 (empty block, streak 2, weight 24)
    dag.insert(id(1), vec![genesis], U256::from_u64(12), 12, 1, ())
        .unwrap();
    dag.insert(id(2), vec![id(1)], U256::from_u64(24), 24, 2, ())
        .unwrap();
    assert_eq!(dag.streak(&id(2)), Some(2));
    assert_eq!(dag.weight(&id(2)), Some(24));
    assert_eq!(dag.parent_streak(&[id(2)]), Ok(2));
    // parallel block b (parent the genesis block, empty block, streak 1): lower chain work than e2, so e2 is the
    // selected parent and the streak follows e2
    dag.insert(id(3), vec![genesis], U256::from_u64(12), 12, 1, ())
        .unwrap();
    assert_eq!(dag.selected_parent(&[id(2), id(3)]), Ok(id(2)));
    assert_eq!(dag.parent_streak(&[id(2), id(3)]), Ok(2));
    assert_eq!(
        dag.parent_streak(&[id(3), id(2)]),
        Ok(2),
        "independent of the parent order"
    );
    // non-empty block n follows e2 and merges b: streak 0; a later empty block starts again from streak 1
    dag.insert(id(4), vec![id(2), id(3)], U256::ONE, 1, 0, ())
        .unwrap();
    assert_eq!(dag.streak(&id(4)), Some(0));
    assert_eq!(dag.parent_streak(&[id(4)]), Ok(0));
    // empty block b2 after b (streak 2): lower chain work than n, so a block with n as selected parent takes its
    // streak from n, and the merged b2 does not count
    dag.insert(id(5), vec![id(3)], U256::from_u64(24), 24, 2, ())
        .unwrap();
    assert_eq!(dag.selected_parent(&[id(4), id(5)]), Ok(id(4)));
    assert_eq!(dag.parent_streak(&[id(4), id(5)]), Ok(0));
    assert_eq!(dag.parent_streak(&[]), Err(GhostdagError::NoParents));
    assert_eq!(
        dag.parent_streak(&[id(9)]),
        Err(GhostdagError::UnknownParent(id(9)))
    );
    assert_eq!(dag.selected_parent(&[]), Err(GhostdagError::NoParents));
    assert_eq!(dag.streak(&id(9)), None);
    assert_eq!(dag.weight(&id(9)), None);
}

/// D-100: the selected parent goes by chain work, the blue work plus the parent's own work. A heavy empty block weighs
/// as much as the cheap blocks it took the time of as soon as it exists: two weight-1 blocks with more blue work no
/// longer outweigh a block of weight 120 before a block is built on it. The merge set keeps the `(blue_work, hash)`
/// order, which stays topological.
#[test]
fn the_selected_parent_counts_its_own_work() {
    let genesis = id(0);
    let mut dag = BlockDag::new(params(8), genesis, U256::ONE, ());
    // a fork point f, then a heavy empty block h (weight 120) on one side and two weight-1 blocks l1 → l2 on the other
    dag.insert(id(1), vec![genesis], U256::ONE, 1, 0, ())
        .unwrap();
    dag.insert(id(2), vec![id(1)], U256::from_u64(120), 120, 1, ())
        .unwrap();
    dag.insert(id(3), vec![id(1)], U256::ONE, 1, 0, ()).unwrap();
    dag.insert(id(4), vec![id(3)], U256::ONE, 1, 0, ()).unwrap();
    // blue work leaves the own work out: l2 has more, as before D-100
    let blue = |h: u64| dag.data(&id(h)).unwrap().blue_work;
    assert_eq!((blue(2), blue(4)), (U256::from_u64(2), U256::from_u64(3)));
    assert!(dag.sort_key(&id(4)).unwrap() > dag.sort_key(&id(2)).unwrap());
    // chain work counts it: h 2 + 120, l2 3 + 1
    assert_eq!(dag.chain_work(&id(2)), Ok(U256::from_u64(122)));
    assert_eq!(dag.chain_work(&id(4)), Ok(U256::from_u64(4)));
    assert_eq!(dag.chain_key(&id(2)), Ok((U256::from_u64(122), id(2))));
    assert_eq!(dag.selected_parent(&[id(4), id(2)]), Ok(id(2)));
    assert_eq!(dag.parent_streak(&[id(4), id(2)]), Ok(1));
    let merged = dag.compute(&[id(2), id(4)], 1).unwrap();
    assert_eq!(merged.selected_parent, Some(id(2)));
    assert_eq!(merged.mergeset_ordered, vec![id(3), id(4)]);
    assert_eq!(merged.blue_work, U256::from_u64(2 + 120 + 1 + 1));
    // with equal chain work the greater hash wins
    dag.insert(id(5), vec![id(1)], U256::from_u64(122 - 2), 1, 0, ())
        .unwrap();
    assert_eq!(dag.chain_work(&id(5)), dag.chain_work(&id(2)));
    assert_eq!(dag.selected_parent(&[id(2), id(5)]), Ok(id(2).max(id(5))));
    // unknown blocks
    assert_eq!(
        dag.selected_parent(&[id(2), id(9)]),
        Err(GhostdagError::UnknownParent(id(9)))
    );
    assert!(matches!(
        dag.chain_key(&id(9)),
        Err(GhostdagError::Corrupt(_))
    ));
}

/// D-100 with D-097: the selected-chain tips are indexed by the fork-choice key `(chain_work, hash)`, so a heavy tip
/// with less blue work is visited first, a tip leaves the index when a block takes it as selected parent, and a
/// restored checkpoint rebuilds the same index.
#[test]
fn selected_chain_tips_follow_the_fork_choice_key() {
    let genesis = id(0);
    let mut dag = BlockDag::new(params(8), genesis, U256::ONE, ());
    // two weight-1 blocks l1 → l2 on one side, a block h of weight 5 on the other
    dag.insert(id(1), vec![genesis], U256::ONE, 1, 0, ())
        .unwrap();
    dag.insert(id(2), vec![id(1)], U256::ONE, 1, 0, ()).unwrap();
    dag.insert(id(3), vec![genesis], U256::from_u64(5), 5, 1, ())
        .unwrap();
    let tips = |dag: &BlockDag<()>| -> Vec<(U256, Hash)> {
        dag.selected_chain_tips_desc(None).copied().collect()
    };
    let key = |work: u64, h: u64| (U256::from_u64(work), id(h));
    // l2 has more blue work (2 against 1), h more chain work (1 + 5 against 2 + 1)
    assert!(dag.sort_key(&id(2)).unwrap() > dag.sort_key(&id(3)).unwrap());
    assert_eq!(tips(&dag), vec![key(6, 3), key(3, 2)]);
    // a search for a better candidate than l2 visits h only, and nothing is above h
    let above: Vec<(U256, Hash)> = dag
        .selected_chain_tips_desc(Some(&key(3, 2)))
        .copied()
        .collect();
    assert_eq!(above, vec![key(6, 3)]);
    assert_eq!(dag.selected_chain_tips_desc(Some(&key(6, 3))).count(), 0);
    // a block on l2 replaces it
    dag.insert(id(4), vec![id(2)], U256::ONE, 1, 0, ()).unwrap();
    assert_eq!(tips(&dag), vec![key(6, 3), key(4, 4)]);
    // a block merging both takes h as its selected parent (blue work 1 + 5 + 3): h leaves the index, the merged tip
    // stays
    dag.insert(id(5), vec![id(4), id(3)], U256::ONE, 1, 0, ())
        .unwrap();
    assert_eq!(dag.data(&id(5)).unwrap().selected_parent, Some(id(3)));
    assert_eq!(tips(&dag), vec![key(10, 5), key(4, 4)]);
    // a restored checkpoint rebuilds the same index
    let restored = BlockDag::restore(params(8), dag.checkpoint().unwrap()).unwrap();
    assert_eq!(tips(&restored), tips(&dag));
    assert!(restored == dag);
}
