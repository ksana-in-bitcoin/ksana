//! Adversarial simulation: reversing a fast-confirmed payment with a private branch, with and without heavy empty
//! blocks (two-speed difficulty, D-084, D-090; the SPEC §15 verification gap "empty-block strategy / selfish-mining
//! analysis", M26).
//!
//! An op is fast-confirmed once a main-chain block includes it (SPEC §10.4), and until it is determined final (§6.5,
//! about an hour) the main chain follows the fork choice of §6.4. An attacker who wants a payment back mines a private
//! branch from the payment block's parent whose first block carries a conflicting op (same account and nonce, or the
//! same balance), and publishes the branch as soon as the fork choice prefers it: the branch becomes the selected
//! chain, its op executes first and the payment, now only in merged blocks, is skipped.
//!
//! Model (time unit = `TARGET_BLOCK_TIME`, 5 seconds: the whole network finds one solution of the base target per unit
//! on average, which ASERT maintains; `bits` is constant during a race of at most an hour):
//!
//! - The attacker has a share `alpha` of the hashrate, the honest miners the rest and are perfectly connected. A block
//!   of weight w needs w times the work (effective target `target / w`), so a party with share s finds weight-w blocks
//!   at rate s / w per unit.
//! - At time 0 the payment is broadcast. The honest miners put it into P1 (weight 1); the attacker starts its branch on
//!   the same parent with A1 (weight 1, the conflicting op).
//! - Honest blocks after P1 ([`Honest`]): busy chain = weight 1 (other users keep blocks non-empty); idle chain = empty
//!   blocks with the streak weights 12, 24, 48, 96, 120, 120, ... (§6.6); and, as a local-policy option, weight 1 for a
//!   while after P1 and idle afterwards.
//! - Attacker blocks after A1 ([`Attacker`]): weight 1 (it stuffs its own ops, which D-087 allows), empty blocks with
//!   the streak weights of its own branch, or empty blocks only while behind.
//! - The merchant accepts `wait` after P1 appeared. The attacker succeeds when its branch is preferred at some moment
//!   in `[P1 + wait, P1 + horizon)`. The horizon is one hour: by then the payment's data has landed on BTC with
//!   `BTC_FINALITY_CONFS` confirmations and is final.
//! - The fork choice is GHOSTDAG's (§6.4, §6.6): the tip with the larger `(chain_work, hash)`, where a block's chain
//!   work is its blue work (the blue blocks in its past) plus its own work (D-100); equal work goes to the larger
//!   hash, with no "first seen" rule, so a tie is a fair coin for the attacker. The rule before D-100 (M26) compared
//!   `blue_work` alone, so the newest block of each branch did not count until a block was built on it; the tables
//!   keep it for comparison ([`ForkChoice::BlueWork`]).
//!
//! Work is the real block work of this crate (`effective_target` / `work_from_target`), so a weight-w block is worth
//! about, but not exactly, w weight-1 blocks. One test replays sample races through the real [`BlockDag`] and checks
//! that GHOSTDAG picks the same tip after every block, and that after a successful race the block merging both
//! branches has the attacker's tip as selected parent and the payment block in its merge set (executed after A1).
//!
//! Findings (M27, D-100, `GHOSTDAG_K` 16; `print_reversal_table` prints the full table, with the rule before D-100 for
//! comparison, and SPEC §10.4 summarizes it):
//!
//! - Weight-1 race (all blocks weight 1, the reference): with the hash tie-break the attacker reverses a 0-second
//!   payment with probability α + 2α² (12% for α = 0.1); after one minute well under 1%, after five minutes nothing
//!   measurable for α ≤ 0.2. Counting a block's own work changes nothing here.
//! - Busy chain against heavy empty blocks: lumpy work wins short races far more often than steady work (the
//!   "difficulty raising" effect, bounded by `MAX_EMPTY_BLOCK_WEIGHT`): a 10% attacker mining empty blocks while
//!   behind wins 6.5% of the races after one minute and about 0.8% after five. Its empty block counts as soon as it is
//!   found, so this is worse than before D-100 (3.5% and 0.6%), when it needed one more block on top.
//! - Idle chain: the honest work arrives in lumps (12, 24, 48, ... per block), so an idle-period payment is protected
//!   by little more than its own block for minutes: a 10% attacker wins a third of the races after one minute and 13%
//!   after five. Before D-100 the newest honest lump did not count yet, and weight-1 blocks won 73% and 36%.

use ksana_consensus::{effective_target, work_from_target, BlockDag, GhostdagParams, Hash, U256};
use ksana_primitives::{Params, MAINNET};

/// One hour in units of `TARGET_BLOCK_TIME` (5 s).
const HOUR: f64 = 720.0;
/// The waiting times after P1 that the tables report: 0, 30 s, 1 min, 5 min, 10 min, 30 min.
const WAITS: [f64; 6] = [0.0, 6.0, 12.0, 60.0, 120.0, 360.0];
/// A target well below the mainnet PoW limit, so that dividing it by any weight keeps the work proportional.
const BITS: u32 = 0x1d00_ffff;

/// Deterministic pseudo-random numbers (splitmix64), as in `convergence.rs`.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1].
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    /// Exponential with rate `rate`.
    fn exponential(&mut self, rate: f64) -> f64 {
        -self.unit().ln() / rate
    }

    /// A random block hash whose first 8 bytes are `prefix` (big-endian), so that two such hashes compare like their
    /// prefixes.
    fn hash_with_prefix(&mut self, prefix: u64) -> Hash {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&prefix.to_be_bytes());
        for chunk in h[8..].chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        h
    }
}

/// The work of a block of each possible weight, from the consensus functions.
struct Works {
    params: &'static Params,
    by_weight: Vec<(u64, U256)>,
}

impl Works {
    fn new(params: &'static Params) -> Self {
        let mut weights = vec![1];
        for streak in 1..=8 {
            let w = params.empty_block_weight(streak);
            if !weights.contains(&w) {
                weights.push(w);
            }
        }
        let by_weight = weights
            .into_iter()
            .map(|w| {
                let target = effective_target(BITS, params.pow_limit_bits, w).unwrap();
                (w, work_from_target(&target).unwrap())
            })
            .collect();
        Works { params, by_weight }
    }

    fn of(&self, weight: u64) -> U256 {
        self.by_weight
            .iter()
            .find(|(w, _)| *w == weight)
            .map(|(_, work)| *work)
            .unwrap()
    }

    /// The weight of the next block of a branch with `streak` consecutive empty blocks, if that block is empty.
    fn empty(&self, streak: u64) -> u64 {
        self.params.empty_block_weight(streak + 1)
    }
}

/// What the honest miners produce after P1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Honest {
    /// Other users' ops keep every honest block non-empty (weight 1).
    Busy,
    /// No other ops: after P1 the honest miners produce empty blocks (weights 12, 24, 48, 96, 120, 120, ...).
    Idle,
    /// A local-policy option, not what producers do today: after P1 the honest producers keep making weight-1 blocks
    /// (with an op of their own) for this many time units, then go idle.
    BusyFor(u32),
}

/// What the attacker produces after A1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attacker {
    /// Weight-1 blocks only.
    Light,
    /// Empty blocks, the weight escalating with the branch's streak.
    Heavy,
    /// An empty block while even the branch's full work (tip included) is not ahead of the honest branch's counted
    /// work, a weight-1 block otherwise: under [`ForkChoice::BlueWork`] a tip's own work only counts once a block is
    /// built on it, and a weight-1 block is the quickest way to make a heavy tip count; under the consensus rule it
    /// keeps a lead with less variance.
    CashIn,
}

/// Which work the fork choice compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForkChoice {
    /// The consensus rule (§6.4, D-100): tips by `(chain_work, hash)`, a block's own work included.
    Consensus,
    /// The rule before D-100 (M26), for comparison: tips by `(blue_work, hash)`, where a block's own work does not
    /// count until a block is built on it.
    BlueWork,
}

#[derive(Clone, Copy, Debug)]
struct Scenario {
    alpha: f64,
    honest: Honest,
    attacker: Attacker,
    rule: ForkChoice,
}

fn scenario(alpha: f64, honest: Honest, attacker: Attacker) -> Scenario {
    Scenario {
        alpha,
        honest,
        attacker,
        rule: ForkChoice::Consensus,
    }
}

/// One block of a race, for the GHOSTDAG replay.
#[derive(Clone, Debug)]
struct Event {
    attacker: bool,
    weight: u64,
    streak: u64,
    /// The first 8 bytes of the block hash (big-endian).
    hash_prefix: u64,
    /// Whether the attacker's branch is preferred after this block (the race model's verdict).
    attacker_preferred: bool,
}

/// A branch from the fork point.
struct Branch {
    blocks: u64,
    streak: u64,
    /// The work of all blocks of the branch.
    work: U256,
    /// The work of the branch's last block.
    tip_work: U256,
    /// The first 8 bytes (big-endian) of the tip's hash; two random hashes compare like these.
    tip: u64,
}

impl Branch {
    fn new() -> Self {
        Branch {
            blocks: 0,
            streak: 0,
            work: U256::ZERO,
            tip_work: U256::ZERO,
            tip: 0,
        }
    }

    /// The fork-choice key of the branch tip relative to the fork point; `None` for an empty branch, whose tip is the
    /// fork point itself, behind any non-empty branch.
    fn key(&self, rule: ForkChoice) -> Option<(U256, u64)> {
        let counted = match rule {
            ForkChoice::Consensus => self.work,
            ForkChoice::BlueWork => self.work.checked_sub(self.tip_work).unwrap(),
        };
        (self.blocks > 0).then_some((counted, self.tip))
    }

    fn add(&mut self, works: &Works, weight: u64, hash_prefix: u64) {
        self.streak = if weight > 1 { self.streak + 1 } else { 0 };
        self.blocks += 1;
        self.tip_work = works.of(weight);
        self.work = self.work.checked_add(self.tip_work).unwrap();
        self.tip = hash_prefix;
    }
}

/// Whether the virtual block would select the attacker's tip.
fn attacker_preferred(attacker: &Branch, honest: &Branch, rule: ForkChoice) -> bool {
    match (attacker.key(rule), honest.key(rule)) {
        (Some(a), Some(h)) => a > h,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Runs one race and returns the end of the last interval (relative to P1) during which the attacker's branch was
/// preferred, capped at the horizon; `None` if it never was after P1. The attacker succeeds for every wait below it.
fn race(
    works: &Works,
    scenario: Scenario,
    horizon: f64,
    rng: &mut SplitMix64,
    mut log: Option<&mut Vec<Event>>,
) -> Option<f64> {
    let Scenario {
        alpha,
        honest: honest_policy,
        attacker: attacker_policy,
        rule,
    } = scenario;
    let mut honest = Branch::new();
    let mut attacker = Branch::new();
    let mut now = 0.0;
    let mut p1: Option<f64> = None;
    let mut last_end: Option<f64> = None;
    loop {
        // the honest template stays as it is until `switch_at`
        let mut switch_at = f64::INFINITY;
        let honest_weight = match (honest_policy, p1) {
            (_, None) | (Honest::Busy, _) => 1,
            (Honest::Idle, _) => works.empty(honest.streak),
            (Honest::BusyFor(units), Some(p1)) => {
                let busy_until = p1 + f64::from(units);
                if now < busy_until {
                    switch_at = busy_until;
                    1
                } else {
                    works.empty(honest.streak)
                }
            }
        };
        let honest_counted = honest.key(rule).map_or(U256::ZERO, |(work, _)| work);
        let attacker_weight = match attacker_policy {
            _ if attacker.blocks == 0 => 1,
            Attacker::Light => 1,
            Attacker::Heavy => works.empty(attacker.streak),
            Attacker::CashIn if attacker.work > honest_counted => 1,
            Attacker::CashIn => works.empty(attacker.streak),
        };
        let honest_rate = (1.0 - alpha) / honest_weight as f64;
        let attacker_rate = alpha / attacker_weight as f64;
        let next = now + rng.exponential(honest_rate + attacker_rate);
        // nothing changes on [now, until)
        let until = next.min(switch_at);
        if let Some(p1) = p1 {
            let end = p1 + horizon;
            if attacker_preferred(&attacker, &honest, rule) {
                last_end = Some(until.min(end) - p1);
            }
            if until >= end {
                return last_end;
            }
        }
        if next > switch_at {
            // the honest template changes before the next block; the exponential clock is memoryless
            now = switch_at;
            continue;
        }
        now = next;
        let honest_found = rng.unit() * (honest_rate + attacker_rate) <= honest_rate;
        let hash_prefix = rng.next_u64();
        let (branch, weight) = if honest_found {
            (&mut honest, honest_weight)
        } else {
            (&mut attacker, attacker_weight)
        };
        branch.add(works, weight, hash_prefix);
        let streak = branch.streak;
        if honest_found && p1.is_none() {
            p1 = Some(now);
        }
        if let Some(log) = log.as_deref_mut() {
            log.push(Event {
                attacker: !honest_found,
                weight,
                streak,
                hash_prefix,
                attacker_preferred: attacker_preferred(&attacker, &honest, rule),
            });
        }
    }
}

/// The share of `trials` races the attacker wins for each wait in [`WAITS`].
fn reversal_rates(works: &Works, scenario: Scenario, trials: u32, seed: u64) -> [f64; 6] {
    let mut rng = SplitMix64(seed);
    let mut wins = [0u32; 6];
    for _ in 0..trials {
        if let Some(end) = race(works, scenario, HOUR, &mut rng, None) {
            for (win, wait) in wins.iter_mut().zip(WAITS) {
                if end > wait {
                    *win += 1;
                }
            }
        }
    }
    wins.map(|w| f64::from(w) / f64::from(trials))
}

fn format_row(label: &str, rates: &[f64; 6]) -> String {
    let cells: Vec<String> = rates
        .iter()
        .map(|r| format!("{:>8.3}%", r * 100.0))
        .collect();
    format!("{label:<44}{}", cells.join(" "))
}

fn print_table(trials: u32) {
    let works = Works::new(&MAINNET);
    let header = ["0 s", "30 s", "1 min", "5 min", "10 min", "30 min"].map(|w| format!("{w:>9}"));
    println!("{:<44}{}", "reversal probability", header.join(" "));
    let regimes = [
        Honest::Busy,
        Honest::Idle,
        Honest::BusyFor(24),
        Honest::BusyFor(60),
    ];
    for rule in [ForkChoice::Consensus, ForkChoice::BlueWork] {
        for honest in regimes {
            for attacker in [Attacker::Light, Attacker::Heavy, Attacker::CashIn] {
                for alpha in [0.05, 0.1, 0.2, 0.3] {
                    let s = Scenario {
                        alpha,
                        honest,
                        attacker,
                        rule,
                    };
                    let rates = reversal_rates(&works, s, trials, 7);
                    let label = format!("{rule:?}/{honest:?}/{attacker:?} a={alpha}");
                    println!("{}", format_row(&label, &rates));
                }
            }
        }
    }
}

/// The weights and works the simulation uses are the consensus values: weight 1 and the streak weights 12 ... 120, and
/// a weight-w block is worth w weight-1 blocks up to rounding.
#[test]
fn the_simulation_uses_the_consensus_weights_and_work() {
    let works = Works::new(&MAINNET);
    let weights: Vec<u64> = works.by_weight.iter().map(|(w, _)| *w).collect();
    assert_eq!(weights, vec![1, 12, 24, 48, 96, 120]);
    let unit = works.of(1);
    for (w, work) in &works.by_weight {
        let scaled = unit.checked_mul_u64(*w).unwrap();
        // ⌊2^256 / (⌊target / w⌋ + 1)⌋ is within w of w × ⌊2^256 / (target + 1)⌋ for this target
        let diff = if *work > scaled {
            work.checked_sub(scaled).unwrap()
        } else {
            scaled.checked_sub(*work).unwrap()
        };
        assert!(diff <= U256::from_u64(*w), "weight {w}");
    }
}

/// Sanity check of the race model against the closed form of a weight-1 race (busy honest chain, light attacker, no
/// wait). At P1 the attacker has k blocks with probability (1 − α)·α^k. Equal blue work is decided by the tip hash, a
/// fair coin each time a new tip creates the tie, so from a tie the attacker wins with probability
/// c = 1/2 + (α + α·c)/2, i.e. c = (1 + α) / (2 − α), and it reaches a tie from a deficit of one block with probability
/// r = α / (1 − α). Summing (1 − α)·r·c + (1 − α)·α·c + α² gives α + 2α² (12% for α = 0.1, against α² / (1 − α) + 2α²
/// = 3.1% if a tie went to the block seen first, as in Bitcoin).
#[test]
fn a_weight_one_race_matches_the_closed_form() {
    let works = Works::new(&MAINNET);
    for (alpha, seed) in [(0.1, 1), (0.2, 2), (0.3, 3)] {
        let trials = 40_000;
        let mut rng = SplitMix64(seed);
        let s = scenario(alpha, Honest::Busy, Attacker::Light);
        let wins = (0..trials)
            .filter(|_| race(&works, s, 4.0 * HOUR, &mut rng, None).is_some())
            .count();
        let measured = wins as f64 / f64::from(trials);
        let expected = alpha + 2.0 * alpha * alpha;
        let sigma = (expected * (1.0 - expected) / f64::from(trials)).sqrt();
        assert!(
            (measured - expected).abs() < 4.0 * sigma,
            "alpha {alpha}: measured {measured}, expected {expected}"
        );
    }
}

/// Busy chain: a weight-1 race against a 10% or 20% attacker is practically over after five minutes, but an attacker
/// that mines empty blocks while behind (each counting at once, D-100) still wins a few percent of the races after one
/// minute and keeps a real chance after five.
#[test]
fn heavy_empty_blocks_let_a_minority_reverse_busy_chain_payments() {
    let works = Works::new(&MAINNET);
    let trials = 20_000;
    for (alpha, seed) in [(0.1, 11), (0.2, 12)] {
        let light = reversal_rates(
            &works,
            scenario(alpha, Honest::Busy, Attacker::Light),
            trials,
            seed,
        );
        let heavy = reversal_rates(
            &works,
            scenario(alpha, Honest::Busy, Attacker::CashIn),
            trials,
            seed,
        );
        println!("{}", format_row(&format!("busy/light a={alpha}"), &light));
        println!("{}", format_row(&format!("busy/cash-in a={alpha}"), &heavy));
        // after one minute (index 2) and after five (index 3)
        assert!(heavy[2] > 0.02, "alpha {alpha}: {heavy:?}");
        assert!(
            heavy[2] > 5.0 * light[2],
            "alpha {alpha}: {heavy:?} vs {light:?}"
        );
        assert!(heavy[3] > 0.003, "alpha {alpha}: {heavy:?}");
        assert!(light[3] < 0.0005, "alpha {alpha}: {light:?}");
        // the chance only falls with the wait
        assert!(heavy.windows(2).all(|w| w[0] >= w[1]));
    }
}

/// Idle chain: after an idle-period payment the honest miners' next block is an empty block of weight 12 (about a
/// minute), the next one 24 (about two more), so the payment is protected by little more than its own block for
/// minutes. Under the rule before D-100 the newest heavy block did not count until a block was built on it, and a 10%
/// attacker stuffing its own ops into weight-1 blocks won most races after one minute and a third after five; counting
/// a block's own work takes that to a third after one minute and a few percent after five, while an attacker mining
/// empty blocks itself keeps about one race in eight after five minutes. A producer policy that keeps weight-1 blocks
/// coming for five minutes after a payment takes the light attacker's chance to the busy-chain level, but not the heavy
/// attacker's, which needs weight-1 blocks until the payment is BTC-final.
#[test]
fn an_idle_chain_leaves_a_new_payment_weakly_protected() {
    let works = Works::new(&MAINNET);
    let alpha = 0.1;
    let idle = |attacker, rule| {
        let s = Scenario {
            rule,
            ..scenario(alpha, Honest::Idle, attacker)
        };
        reversal_rates(&works, s, 20_000, 21)
    };
    let before = idle(Attacker::Light, ForkChoice::BlueWork);
    let light = idle(Attacker::Light, ForkChoice::Consensus);
    let cash_in = idle(Attacker::CashIn, ForkChoice::Consensus);
    println!("{}", format_row("idle/light a=0.1, before D-100", &before));
    println!("{}", format_row("idle/light a=0.1", &light));
    println!("{}", format_row("idle/cash-in a=0.1", &cash_in));
    // after one minute (index 2) and after five (index 3)
    assert!(before[2] > 0.5 && before[3] > 0.2, "{before:?}");
    assert!(light[2] < 0.6 * before[2], "{light:?}");
    assert!(light[3] < 0.08 && light[3] * 4.0 < before[3], "{light:?}");
    assert!(cash_in[3] > 0.06, "{cash_in:?}");
    // a five-minute busy window after the payment
    let [light, cash_in] = [Attacker::Light, Attacker::CashIn].map(|attacker| {
        reversal_rates(
            &works,
            scenario(alpha, Honest::BusyFor(60), attacker),
            20_000,
            22,
        )
    });
    println!("{}", format_row("busy for 5 min/light a=0.1", &light));
    println!("{}", format_row("busy for 5 min/cash-in a=0.1", &cash_in));
    assert!(light[3] < 0.002, "{light:?}");
    assert!(cash_in[3] > 0.01, "{cash_in:?}");
}

/// Replays sample races through the real GHOSTDAG: after every block the virtual selected parent of the two branch tips
/// is the attacker's tip exactly when the race model says its branch is preferred, and when the attacker publishes a
/// preferred branch, a block merging both branches selects the attacker's tip and has the payment block P1 in its merge
/// set, so P1's entries execute after the conflicting op in A1 (SPEC §6.6, §9.6).
#[test]
fn the_race_model_agrees_with_ghostdag() {
    let works = Works::new(&MAINNET);
    let params = GhostdagParams::from_params(&MAINNET);
    let mut rng = SplitMix64(31);
    let mut checked_wins = 0;
    for (honest_policy, attacker_policy, alpha) in [
        (Honest::Busy, Attacker::CashIn, 0.25),
        (Honest::Busy, Attacker::Heavy, 0.3),
        (Honest::Idle, Attacker::Light, 0.15),
        (Honest::BusyFor(24), Attacker::CashIn, 0.3),
    ] {
        for _ in 0..150 {
            let mut log = Vec::new();
            race(
                &works,
                scenario(alpha, honest_policy, attacker_policy),
                HOUR,
                &mut rng,
                Some(&mut log),
            );
            let genesis = rng.hash_with_prefix(0);
            let mut dag = BlockDag::new(params, genesis, works.of(1), ());
            let fork = rng.hash_with_prefix(1);
            dag.insert(fork, vec![genesis], works.of(1), 1, 0, ())
                .unwrap();
            let (mut honest_tip, mut attacker_tip) = (fork, fork);
            let (mut p1, mut a1) = (None, None);
            for (i, event) in log.iter().enumerate() {
                let parent = if event.attacker {
                    attacker_tip
                } else {
                    honest_tip
                };
                // the streak and the weight follow the selected parent as the consensus computes them (D-090)
                if event.weight > 1 {
                    assert_eq!(event.streak, dag.parent_streak(&[parent]).unwrap() + 1);
                    assert_eq!(event.weight, MAINNET.empty_block_weight(event.streak));
                } else {
                    assert_eq!(event.streak, 0);
                }
                let hash = rng.hash_with_prefix(event.hash_prefix);
                dag.insert(
                    hash,
                    vec![parent],
                    works.of(event.weight),
                    event.weight,
                    event.streak,
                    (),
                )
                .unwrap();
                if event.attacker {
                    attacker_tip = hash;
                    a1.get_or_insert(hash);
                } else {
                    honest_tip = hash;
                    p1.get_or_insert(hash);
                }
                if attacker_tip == fork || honest_tip == fork {
                    continue;
                }
                let selected = dag.selected_parent(&[honest_tip, attacker_tip]).unwrap();
                assert_eq!(
                    selected == attacker_tip,
                    event.attacker_preferred,
                    "block {i} of a {honest_policy:?}/{attacker_policy:?} race"
                );
                // the attacker publishes once its branch is preferred; the honest miners then merge both branches
                let honest_branch = log[..=i].iter().filter(|e| !e.attacker).count();
                if event.attacker_preferred && honest_branch < params.max_mergeset_size {
                    let merged = dag.compute(&[attacker_tip, honest_tip], 1).unwrap();
                    assert_eq!(merged.selected_parent, Some(attacker_tip));
                    assert!(merged.mergeset_ordered.contains(&p1.unwrap()));
                    assert!(dag.in_closed_past(&a1.unwrap(), &attacker_tip).unwrap());
                    checked_wins += 1;
                    break;
                }
            }
        }
    }
    assert!(
        checked_wins > 50,
        "only {checked_wins} successful races replayed"
    );
}

/// The full table, which SPEC §10.4 summarizes (D-098 and D-100 record the runs before and after D-100):
/// `cargo test -p ksana-consensus --release --test reorg_race -- --ignored --nocapture` (`KSANA_RACE_TRIALS` sets the
/// number of races per row, 200 000 by default).
#[test]
#[ignore = "prints the full table; slow"]
fn print_reversal_table() {
    let trials = std::env::var("KSANA_RACE_TRIALS")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(200_000);
    print_table(trials);
}
