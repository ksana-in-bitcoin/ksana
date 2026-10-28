//! M2 acceptance item 1: a single node keeps producing blocks at about 5-second intervals and the difficulty adjustment
//! converges.
//!
//! ASERT's convergence time constant is about the half-life (7 200 seconds, D-090); starting from a difficulty far from
//! the equilibrium it takes hours to converge; convergence itself is covered by the simulation tests of
//! `ksana-consensus` (various hash rates and hash rate jumps). Here we verify on a real node: set the genesis
//! difficulty near the equilibrium according to the PoW speed measured on this machine (expecting one block per 4
//! seconds, slightly easier than the target), the node keeps producing blocks, the elapsed time of the measured
//! interval matches the expected time of each block's difficulty at this machine's speed, every block's `bits` is
//! adjusted block by block by ASERT, and all blocks can be fully validated by another instance.
//!
//! Block intervals follow an exponential distribution: the total time of n intervals divided by the expectation follows
//! Gamma(n, 1) / n, with a coefficient of variation of about 18% at 30 blocks. The average interval used to be checked
//! against a fixed range (3.0-7.5 seconds); with an expectation of 4 seconds the probability of falling below 3.0
//! seconds is about 7.5% (CI run 57 measured 2.73 seconds and failed); the bounds are now taken from the gamma
//! distribution ([`TAIL`] on each side), with room left for the error of the PoW speed measurement.
//!
//! Two-speed difficulty (D-084, D-090): an empty block's effective target is 1/w of a non-empty block's, w = 12, 24,
//! ... by streak, expected time w times as long. The test keeps submitting path-B transfers (about one every 250 ms) so
//! that almost every block is non-empty; each block's expected time is computed from its own weight (as recorded in
//! the DAG), so the occasional empty block is counted correctly too.
//!
//! This file holds only this one test, occupying a test process by itself to avoid competing for CPU with other
//! block-producing nodes.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use ksana_consensus::{Asert, Consensus};
use ksana_node::chain::Chain;
use ksana_node::genesis::GenesisFile;
use ksana_primitives::DEVNET;

const WARMUP_BLOCKS: u64 = 6;
const MEASURED_BLOCKS: u64 = 30;
/// The probability (each) that the measured interval's elapsed time is below the lower bound or above the upper bound.
const TAIL: f64 = 5e-6;
/// Error of the PoW speed measurement: it is measured for only 2 seconds in the test process, while the node produces
/// blocks for over two minutes in another process, and the speed of CI machines fluctuates.
const SPEED_TOLERANCE: f64 = 1.3;
/// Interval between transfer submissions.
const TRAFFIC_INTERVAL: Duration = Duration::from_millis(250);

#[test]
fn single_node_produces_blocks_about_every_five_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let btc = TestBtc::new(dir.path());
    let data = dir.path().join("data");
    let (alice, bob, producer) = (user("alice"), user("bob"), user("producer"));
    let rate = pow_rate();
    let genesis = write_genesis(
        dir.path(),
        btc.anchor(),
        &[(&alice, 1_000_000_000)],
        Some(bits_for_seconds(rate, 4.0)),
    );
    let rt = rt();
    let node = NodeProcess::start(&data, &genesis, Some(&producer), 1, &btc);
    let c = node.client();

    // Keep submitting transfers (consecutive nonces; on a failed submit retry the same nonce later) so blocks are
    // non-empty
    let stop = Arc::new(AtomicBool::new(false));
    let traffic = {
        let stop = Arc::clone(&stop);
        let client = node.client();
        let to = bob.account;
        std::thread::spawn(move || {
            let rt = common::rt();
            let mut nonce = 0;
            while !stop.load(Ordering::Relaxed) {
                if submit(&rt, &client, &transfer_op(&alice, nonce, 5, to, 1)).is_ok() {
                    nonce += 1;
                }
                std::thread::sleep(TRAFFIC_INTERVAL);
            }
        })
    };

    let timeout = Duration::from_secs(60 + 20 * (WARMUP_BLOCKS + MEASURED_BLOCKS));
    let start = wait_height(&rt, &c, WARMUP_BLOCKS, timeout);
    let observed_from = ksana_node::monotonic_now();
    let end = wait_height(&rt, &c, WARMUP_BLOCKS + MEASURED_BLOCKS, timeout);
    let wall = observed_from.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    traffic.join().unwrap();
    node.stop();

    let genesis_file = GenesisFile::load(&genesis).unwrap();
    let source = Chain::open(&data, DEVNET, &genesis_file, btc.client()).unwrap();

    // Elapsed time of the measured interval (the blocks after start up to end): by block timestamps (the node uses the
    // current time as timestamp); expected time: the mean number of attempts for each block's difficulty and weight
    // (D-084) divided by this machine's PoW speed (including ASERT's block-by-block adjustment within the interval)
    let blocks = end.header.height - start.header.height;
    let by_timestamp = (end.header.timestamp - start.header.timestamp) as f64;
    let mut empty_blocks = 0u64;
    let expected: f64 = (start.header.height + 1..=end.header.height)
        .map(|height| {
            let hash = source.store().hash_at(height).unwrap().unwrap();
            let block = source.store().block(&hash).unwrap().unwrap();
            let weight = source.weight_of(&hash).unwrap().unwrap();
            if weight != 1 {
                empty_blocks += 1;
            }
            expected_attempts(block.header.bits) * weight as f64 / rate
        })
        .sum();
    let ratio = by_timestamp / expected;
    let (low, high) = mean_bounds(blocks, TAIL);
    let (low, high) = (low / SPEED_TOLERANCE, high * SPEED_TOLERANCE);
    let per_block = |secs: f64| secs / blocks as f64;
    eprintln!(
        "average block interval: {:.2} s by timestamps, {:.2} s by observed time, {:.2} s expected by difficulty \
         ({blocks} blocks, {empty_blocks} of them empty); elapsed / expected = {ratio:.2}, bounds {low:.2}-{high:.2}",
        per_block(by_timestamp),
        per_block(wall),
        per_block(expected),
    );
    assert!(
        (low..=high).contains(&ratio),
        "{blocks} blocks took {by_timestamp:.0} s, expected {expected:.0} s by difficulty, the ratio {ratio:.2} is not within {low:.2}-{high:.2}"
    );
    assert!(
        empty_blocks * 2 < blocks,
        "{empty_blocks} of {blocks} blocks are empty: the continuously submitted transfers did not keep blocks non-empty"
    );

    // Every block's bits equals the value ASERT computes from the selected parent's DAA score and timestamp
    // (independent of this block's weight, D-084), and it does change block by block; the DAA score accumulates
    // weights; all blocks can be validated by another instance
    let asert = Asert::from_genesis(&DEVNET, source.genesis()).unwrap();
    let replay_dir = tempfile::tempdir().unwrap();
    let replica = Chain::open(replay_dir.path(), DEVNET, &genesis_file, btc.client()).unwrap();
    while replica.sync_btc().unwrap() {}
    let tip = source.tip().unwrap();
    let mut distinct_bits = std::collections::BTreeSet::new();
    let mut parent = source.genesis().clone();
    let mut parent_daa_score = 0u64;
    for height in 1..=tip.header.height {
        let hash = source.store().hash_at(height).unwrap().unwrap();
        let block = source.store().block(&hash).unwrap().unwrap();
        let weight = source.weight_of(&hash).unwrap().unwrap();
        let daa_score = parent_daa_score + weight;
        assert_eq!(
            source.ghostdag(&hash).unwrap().unwrap().daa_score,
            daa_score,
            "height {height}"
        );
        assert_eq!(
            block.header.bits,
            asert.next_bits(parent_daa_score, parent.timestamp).unwrap()
        );
        assert_eq!(
            block.header.bits,
            source
                .consensus()
                .next_bits(&parent, daa_score, weight)
                .unwrap()
        );
        distinct_bits.insert(block.header.bits);
        replica.import_block(&block, None).unwrap();
        parent = block.header;
        parent_daa_score = daa_score;
    }
    assert!(
        distinct_bits.len() > 1,
        "the difficulty should adjust block by block"
    );
    assert_eq!(replica.tip().unwrap(), tip);
}

/// Quantile bounds of the mean of n independent exponential variables with mean 1: the probabilities that the mean is
/// below `low` and above `high` are each `tail`.
///
/// Their sum follows Gamma(n, 1), and P(sum > x) = P(Poisson with mean x < n).
fn mean_bounds(n: u64, tail: f64) -> (f64, f64) {
    let above = |x: f64| {
        let mut term = (-x).exp();
        let mut sum = 0.0;
        for k in 0..n {
            sum += term;
            term *= x / (k + 1) as f64;
        }
        sum
    };
    // above decreases monotonically in x: bisect for above(x) = p
    let solve = |p: f64| {
        let (mut lo, mut hi) = (0.0, 10.0 * n as f64);
        for _ in 0..200 {
            let mid = (lo + hi) / 2.0;
            if above(mid) > p {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0 / n as f64
    };
    (solve(1.0 - tail), solve(tail))
}
