//! Simulation of difficulty adjustment convergence (M2 acceptance item "a single node keeps producing blocks at about
//! 5-second intervals and the difficulty adjustment converges").
//!
//! A miner with fixed hashrate H (hashes/second) produces blocks starting from the genesis block (devnet PoW limit,
//! work 2). Each block's solve time follows an exponential distribution with mean work / H (work = the expected
//! number of attempts to find a hash meeting the target, D-025); the timestamp is the simulated clock in whole
//! seconds (floor, not forced to increase: consensus does not require it, nodes actually use max(local time,
//! median time + 1), and a separate scenario follows that policy). Each block's `bits` and work are computed entirely
//! by this crate (`Consensus::next_bits` / `Consensus::work`); only the simulation part uses floating point and a
//! hand-written deterministic random number generator (splitmix64).
//!
//! Statistical margin: once converged, ASERT keeps the chain near the schedule; the standard deviation of the mean
//! interval over a 1000-block window is about 0.07 seconds, so ±10% (0.5 seconds) is about 7σ.
//!
//! Two-speed difficulty (D-084, D-090): the effective target of an empty block is the `bits` target divided by its
//! weight, and the weight doubles from N = 12 up to 120 according to the number of consecutive empty blocks on the
//! selected chain. Each block may be empty or non-empty, and convergence is measured by the time per unit of DAA score
//! (the time per block when all blocks are non-empty).
//!
//! Half-life 7 200 seconds (D-090): starting from the genesis block's minimum difficulty, the difficulty doubles per
//! half-life ahead of schedule; when all blocks are non-empty each block is only 5 seconds ahead, and a hashrate of
//! 1e9 H/s needs about 31 half-lives (about 45 000 blocks) ahead to converge, so the scenarios starting from the
//! genesis block produce 65 000 blocks.

use ksana_consensus::{
    check_timestamp, empty_entries_root, genesis_header, median_time_past, Consensus, PowConsensus,
    U256,
};
use ksana_primitives::{BlockHeader, BtcAnchor, DEVNET};

const TARGET: f64 = 5.0;
const WINDOW: usize = 1000;

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Exponential distribution with mean `mean`.
    fn exponential(&mut self, mean: f64) -> f64 {
        let unit = ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64; // (0, 1]
        -unit.ln() * mean
    }
}

fn to_f64(x: U256) -> f64 {
    x.to_limbs().iter().rev().fold(0.0, |acc, &limb| {
        acc * 18_446_744_073_709_551_616.0 + limb as f64
    })
}

struct Chain {
    consensus: PowConsensus,
    tip: BlockHeader,
    genesis_time: u64,
    /// The simulated clock (seconds after the genesis block).
    clock: f64,
    rng: SplitMix64,
    /// The node's timestamp policy: max(clock, median time of the previous 11 blocks + 1).
    median_policy: bool,
    timestamps: Vec<u64>,
    /// Each block's expected solve time, work / H (seconds).
    expected: Vec<f64>,
    /// Each block's weight (D-084, D-090: empty blocks 12, 24, …, 120 by streak, non-empty blocks 1).
    weights: Vec<u64>,
    /// The tip's DAA score.
    daa_score: u64,
    /// The tip's empty-block streak (D-090).
    streak: u64,
}

impl Chain {
    fn new(seed: u64, median_policy: bool) -> Self {
        let genesis_time = 1_700_000_000;
        let anchor = BtcAnchor {
            height: 1,
            hash: [1; 32],
        };
        let genesis = genesis_header(&DEVNET, genesis_time, anchor, [2; 32]).unwrap();
        Self {
            consensus: PowConsensus::new(&DEVNET, &genesis).unwrap(),
            tip: genesis,
            genesis_time,
            clock: 0.0,
            rng: SplitMix64(seed),
            median_policy,
            timestamps: vec![genesis_time],
            expected: vec![0.0],
            weights: vec![1],
            daa_score: 0,
            streak: 0,
        }
    }

    fn mine(&mut self, blocks: usize, hashrate: f64) {
        self.mine_with(blocks, hashrate, |_| false);
    }

    /// Produces `blocks` blocks, the i-th (counting from 1) being empty if and only if `empty(i)`: an empty block's
    /// weight increases with the streak and its effective target is the bits target divided by the weight (D-084,
    /// D-090); `bits` is computed from the selected parent and the DAA score without this block's weight.
    fn mine_with(&mut self, blocks: usize, hashrate: f64, empty: impl Fn(usize) -> bool) {
        for i in 1..=blocks {
            let empty = empty(i);
            let weight = if empty {
                DEVNET.empty_block_weight(self.streak + 1)
            } else {
                1
            };
            let daa_score = self.daa_score + weight;
            let bits = self
                .consensus
                .next_bits(&self.tip, daa_score, weight)
                .unwrap();
            let mut header = BlockHeader {
                height: self.tip.height + 1,
                entries_root: if empty { empty_entries_root() } else { [1; 32] },
                bits,
                ..self.tip.clone()
            };
            assert_eq!(self.consensus.weight(&header, self.streak), weight);
            let mean = to_f64(self.consensus.work(&header, weight).unwrap()) / hashrate;
            self.clock += self.rng.exponential(mean);
            let mut timestamp = self.genesis_time + self.clock as u64;
            if self.median_policy {
                let recent = &self.timestamps[self.timestamps.len().saturating_sub(11)..];
                timestamp = timestamp.max(median_time_past(recent).unwrap() + 1);
            }
            header.timestamp = timestamp;
            self.timestamps.push(timestamp);
            self.expected.push(mean);
            self.weights.push(weight);
            self.daa_score = daa_score;
            self.streak = self.consensus.streak(&header, self.streak);
            self.tip = header;
        }
    }

    /// The mean interval per unit of DAA score (seconds) of the blocks at heights `[from, from + WINDOW)`: the mean
    /// interval per block when all blocks are non-empty.
    fn mean_interval(&self, from: usize) -> f64 {
        let (start, end) = (
            self.timestamps[from - 1],
            self.timestamps[from + WINDOW - 1],
        );
        (end as f64 - start as f64) / self.window_weight(from)
    }

    /// The expected solve time per unit of DAA score (seconds) within the window.
    fn mean_expected(&self, from: usize) -> f64 {
        self.expected[from..from + WINDOW].iter().sum::<f64>() / self.window_weight(from)
    }

    /// The sum of the weights of the blocks within the window.
    fn window_weight(&self, from: usize) -> f64 {
        self.weights[from..from + WINDOW].iter().sum::<u64>() as f64
    }
}

fn assert_converged(chain: &Chain, from: usize, label: &str) {
    assert_converged_within(chain, from, label, 0.1);
}

/// The mean interval per unit of DAA score within the window is within ±10% of the target, and the expected solve time
/// within ±`expected_tolerance` of the target.
fn assert_converged_within(chain: &Chain, from: usize, label: &str, expected_tolerance: f64) {
    let interval = chain.mean_interval(from);
    let expected = chain.mean_expected(from);
    println!(
        "{label}: blocks [{from}, {}) mean interval {interval:.3} s, expected solve time {expected:.3} s",
        from + WINDOW
    );
    assert!(
        (interval - TARGET).abs() <= 0.1 * TARGET,
        "{label}: blocks [{from}, {}) mean interval {interval:.3} s",
        from + WINDOW
    );
    assert!(
        (expected - TARGET).abs() <= expected_tolerance * TARGET,
        "{label}: blocks [{from}, {}) expected solve time {expected:.3} s",
        from + WINDOW
    );
}

/// Hashrates 1e4 … 1e9 H/s: starting from the genesis block (work 2), converges to 5 seconds after about 50 000
/// blocks (at H = 1e9 about 31 half-lives ahead of schedule are needed, 5 seconds ahead per block); after a 10× step
/// in hashrate it first speeds up, then re-converges within about 10 000 blocks (the difficulty has to rise another
/// 3.3×, time constant about 2 000 blocks).
#[test]
fn converges_for_a_wide_range_of_hashrates_and_after_a_10x_step() {
    for (i, exponent) in (4..=9).enumerate() {
        let hashrate = 10f64.powi(exponent);
        let mut chain = Chain::new(0x5eed + i as u64, false);
        chain.mine(65_000, hashrate);
        for from in [55_000, 60_000] {
            assert_converged(&chain, from, &format!("H = 1e{exponent}"));
        }
        // step: the hashrate becomes 10 times
        chain.mine(20_000, 10.0 * hashrate);
        let burst = (chain.timestamps[65_200] - chain.timestamps[65_000]) as f64 / 200.0;
        assert!(
            burst < 0.5 * TARGET,
            "should speed up first after the step: {burst:.3} s"
        );
        for from in [77_000, 80_000, 83_000] {
            assert_converged(&chain, from, &format!("H = 1e{exponent} → 10×"));
        }
    }
}

/// Still converges under the node's timestamp policy max(clock, median time + 1) (early on, many blocks within the
/// same second push the timestamps ahead of the clock).
#[test]
fn converges_with_the_node_timestamp_policy() {
    let mut chain = Chain::new(42, true);
    chain.mine(65_000, 1e9);
    // every timestamp satisfies the lower bound of the acceptance rule (greater than the median time of the previous
    // 11 blocks)
    for n in 1..chain.timestamps.len() {
        let header = BlockHeader {
            timestamp: chain.timestamps[n],
            ..chain.tip.clone()
        };
        let recent = &chain.timestamps[n.saturating_sub(11)..n];
        assert_eq!(
            check_timestamp(&DEVNET, &header, recent, chain.timestamps[n]),
            Ok(())
        );
    }
    for from in [55_000, 60_000] {
        assert_converged(&chain, from, "median time policy");
    }
}

/// D-084 / D-090: with only empty blocks the weights are 12, 24, 48, 96, 120, 120…, and from the 5th block on the
/// time per block converges to N_MAX × 5 = 600 seconds (still 5 seconds per unit of DAA score); with empty and
/// non-empty blocks mixed (single empty blocks or whole streaks) the time per unit of DAA score likewise converges to
/// 5 seconds: the difficulty adjustment only looks at the DAA score, not at the kind of block.
///
/// With only empty blocks each block takes about 600 seconds while ASERT's half-life is 7 200 seconds (the same ratio
/// as D-084's 60 seconds / 600 seconds): the random time of a single block (standard deviation about 600 seconds)
/// alone moves the target by about 6%, the stationary standard deviation of the schedule error is about 1 800 seconds
/// and the difficulty jitters by about ±19%, and the mean expected solve time over a 1000-block window still has a
/// standard deviation of about 3% (the chain itself still advances on schedule). So the tolerance of the expected
/// solve time is relaxed to ±25% here, while the mean interval is still required within ±10%.
#[test]
fn empty_block_streaks_converge_to_the_capped_spacing() {
    let (n, cap) = (DEVNET.empty_block_difficulty, DEVNET.max_empty_block_weight);
    assert_eq!((n, cap), (12, 120));
    let mut chain = Chain::new(0x5_eede, false);
    chain.mine_with(4000, 1e6, |_| true);
    assert_eq!(&chain.weights[1..7], &[12, 24, 48, 96, 120, 120]);
    assert!(chain.weights[5..].iter().all(|w| *w == cap));
    for from in [2000, 3000] {
        assert_converged_within(&chain, from, "all empty blocks", 0.25);
    }
    let per_block = (chain.timestamps[3999] - chain.timestamps[2999]) as f64 / 1000.0;
    let spacing = cap as f64 * TARGET;
    assert!(
        (per_block - spacing).abs() <= 0.1 * spacing,
        "mean interval of empty blocks at the cap {per_block:.2} s, should be about {spacing} s"
    );
    // one empty block every 3 blocks: the streaks are all 1, weight 12. Each block is on average
    // (1 + 1 + 12) × 5 / 3 seconds ahead of schedule, so about 7 000 blocks are needed to converge from the minimum
    // difficulty
    let mut mixed = Chain::new(0x5_eedf, false);
    mixed.mine_with(11_000, 1e6, |i| i % 3 == 0);
    assert!(mixed.weights[1..].iter().all(|w| *w == 1 || *w == n));
    for from in [9000, 10_000] {
        assert_converged(&mixed, from, "empty and non-empty blocks mixed");
    }
    // 7 empty blocks every 8 blocks (streaks 1-7: weights 12, 24, 48, 96, 120, 120, 120), then a non-empty block ends
    // the streak
    let mut streaks = Chain::new(0x5_eee0, false);
    streaks.mine_with(5000, 1e6, |i| i % 8 != 0);
    assert_eq!(&streaks.weights[1..9], &[12, 24, 48, 96, 120, 120, 120, 1]);
    for from in [3000, 4000] {
        assert_converged_within(
            &streaks,
            from,
            "empty-block streaks and non-empty blocks mixed",
            0.25,
        );
    }
}
