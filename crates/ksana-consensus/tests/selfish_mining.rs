//! Adversarial simulation: withholding blocks against GHOSTDAG rewards, with and without heavy empty blocks (two-speed
//! difficulty, D-084, D-090; the SPEC §15 verification gap "empty-block strategy / selfish-mining analysis", M26), on
//! the fork choice of D-100 (M27).
//!
//! Rewards go to the main-chain blocks and to the blue blocks they merge; a red block gets nothing (SPEC §8.6). A merged
//! block is blue only when at most `GHOSTDAG_K` blue blocks are in its anticone and it leaves every blue block's
//! anticone within K (§6.6). So the way to take rewards away from honest miners is a hidden chain that the fork choice
//! prefers once it is published, with more than K blocks on one side of the anticone: more than K hidden blocks against
//! each honest block found meanwhile, or a hidden heavy block against more than K honest blocks. With every block of
//! weight 1 a minority almost never gets that far. Heavy empty blocks change the arithmetic: an empty block is worth at
//! least `EMPTY_BLOCK_DIFFICULTY` (12) weight-1 blocks, more than K + 1 under D-063's K = 8 (D-100 raises K to 16 for
//! that reason), an honest empty block of weight 96 or 120 takes eight to ten minutes, and weight-1 blocks are cheap
//! (the attacker fills them with its own ops, which D-087 allows).
//!
//! Model (time unit = `TARGET_BLOCK_TIME`, 5 seconds: the whole network finds one solution of the base target per unit
//! on average, which ASERT maintains; `bits` is constant during a run):
//!
//! - The attacker has a share `alpha` of the hashrate, the honest miners the rest. A block of weight w needs w times
//!   the work, so a party with share s finds weight-w blocks at rate s / w per unit.
//! - The honest miners are perfectly connected and see the attacker's published blocks at once. They build on all
//!   published tips (at most `MAX_BLOCK_PARENTS`, best first) and publish at once. Their blocks are weight 1 on a busy
//!   chain ([`Load::Busy`]: other users' ops) and empty with the streak weights 12, 24, 48, 96, 120, 120, ... on an
//!   idle chain ([`Load::Idle`]); the streak follows the selected parent (§6.6).
//! - The attacker ([`Strategy`]) mines like the honest miners (the reference), publishes weight-1 blocks at once, mines
//!   weight-1 blocks on a private chain and publishes it when that makes the honest blocks found meanwhile red, or
//!   starts the private chain with an empty block and publishes it just before the honest chain would overtake it; a
//!   private chain the fork choice no longer prefers is published at once (its blocks then are merged like late
//!   blocks).
//! - The fork choice is that of §6.4: tips by `(chain_work, hash)`, a block's own work included (D-100). Under the rule
//!   before it a block's own work did not count until a block was built on it; D-098 records what that rule gave.
//! - Every block goes through the real [`BlockDag`]; the rewards are tallied from the final selected chain and its
//!   coloring, in reward slots (a block of weight w has w slots, D-084, D-090).
//!
//! Findings (M27, `GHOSTDAG_K` 16; `print_selfish_mining_table` prints the full tables, which SPEC §17.4 and D-100
//! summarize):
//!
//! - Mining honestly or publishing weight-1 blocks at once pays each side its hashrate share; nothing turns red. Weight-1
//!   blocks published at once keep resetting the honest empty-block streak, so an idle chain carries many more headers
//!   to BTC (about 6 an hour without them, 71 with a 5% miner publishing weight-1 blocks, 113 with a 10% one).
//! - Withholding weight-1 blocks no longer pays on an idle chain: the private chain has to outweigh the honest heavy
//!   block itself, which it rarely does before it is merged too late to be blue. The honest miners lose at most 1.2% of
//!   their slots (α = 30%) and the attacker a tenth to almost half of its own; before D-100 a 5% / 10% / 20% attacker
//!   took 30% / 45% / 56% of the honest slots at no cost. On a busy chain it does not pay either way: the honest miners
//!   lose less than 0.01% of their slots up to α = 20% and 0.61% at 30%.
//! - Withholding a heavy block is what remains. The attacker's empty block counts at once and is worth about twelve
//!   honest weight-1 blocks, so it publishes just before the honest blocks found meanwhile overtake it, and those beyond
//!   the first K turn red. With K = 16 they mostly fit: on a busy chain a 5% / 10% / 20% / 30% attacker takes 0 / 0.02% /
//!   1.1% / 10% of the honest slots; on an idle chain, where the private chain can still put more than K blocks in the
//!   anticone of an honest empty block, 0.50% / 2.9% / 9.6% / 16%; it loses none of its own blocks. With D-063's K = 8
//!   it took 1.5% / 3.6% / 11% / 23% and 1.6% / 5.4% / 13% / 20%. `KSANA_SM_K` and `KSANA_SM_N` replay other values
//!   (K = 12: 0.58% and 4.0% at α = 10%; `EMPTY_BLOCK_DIFFICULTY` 9 with K = 8: 1.1% and 5.4%; a lower N does nothing for
//!   an idle chain, whose honest blocks are empty blocks too).

use std::collections::BTreeSet;

use ksana_consensus::{effective_target, work_from_target, BlockDag, GhostdagParams, Hash, U256};
use ksana_primitives::{Params, MAINNET};

/// One hour in units of `TARGET_BLOCK_TIME` (5 s).
const HOUR: f64 = 720.0;
/// A target well below the mainnet PoW limit, so that dividing it by any weight keeps the work proportional.
const BITS: u32 = 0x1d00_ffff;
/// The attacker publishes a private chain of this length even when it gains nothing by it, so that the block merging the
/// chain stays within `MAX_MERGESET_SIZE`.
const MAX_PRIVATE: usize = 100;

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

    /// A random block hash.
    fn hash(&mut self) -> Hash {
        let mut h = [0u8; 32];
        for chunk in h.chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        h
    }
}

/// The parameters of a run, and the work of a block of each possible weight from the consensus functions.
struct Works {
    params: Params,
    by_weight: Vec<(u64, U256)>,
}

impl Works {
    fn new(params: &Params) -> Self {
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
        Works {
            params: params.clone(),
            by_weight,
        }
    }

    fn of(&self, weight: u64) -> U256 {
        self.by_weight
            .iter()
            .find(|(w, _)| *w == weight)
            .map(|(_, work)| *work)
            .unwrap()
    }
}

/// What the honest miners' blocks carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Load {
    /// Other users' ops keep every honest block non-empty (weight 1).
    Busy,
    /// No ops: the honest miners produce empty blocks, weighted by the streak of their selected parent.
    Idle,
}

/// What the attacker does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strategy {
    /// Mines and publishes like the honest miners (the reference).
    Honest,
    /// Weight-1 blocks with ops of its own on all published tips, published at once.
    LightPublic,
    /// Weight-1 blocks on a private chain. Once an honest block has been found that the private chain does not have in
    /// its past, the attacker publishes the chain when it is preferred and longer than K blocks (the honest blocks
    /// found meanwhile turn red), or as soon as it is no longer preferred.
    LightWithhold,
    /// An empty block on the published tips (weighted like the honest miners' next empty block: 12 on a busy chain),
    /// kept private and extended with weight-1 blocks. Once the heavy block is the selected parent, the honest blocks
    /// found meanwhile beyond the first K turn red, so the attacker publishes when the private chain is longer than K
    /// blocks, when the next honest block would overtake it, or as soon as it is no longer preferred.
    HeavyWithhold,
}

/// The template of the next block: parents, weight and empty-block streak.
type Template = (Vec<Hash>, u64, u64);

/// The rewards of one run, in blocks and reward slots.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    hours: f64,
    honest_blocks: u64,
    /// Reward slots of the honest blocks (their weights).
    honest_slots: u64,
    /// Reward slots of the honest blocks that are main-chain blocks or blue.
    honest_paid: u64,
    honest_red: u64,
    /// Honest red blocks of weight 96 or more.
    honest_heavy_red: u64,
    /// Honest red blocks of weight 1 (non-empty).
    honest_red_light: u64,
    attacker_blocks: u64,
    attacker_slots: u64,
    attacker_paid: u64,
    attacker_red: u64,
}

impl Tally {
    fn add(&mut self, other: &Tally) {
        self.hours += other.hours;
        self.honest_blocks += other.honest_blocks;
        self.honest_slots += other.honest_slots;
        self.honest_paid += other.honest_paid;
        self.honest_red += other.honest_red;
        self.honest_heavy_red += other.honest_heavy_red;
        self.honest_red_light += other.honest_red_light;
        self.attacker_blocks += other.attacker_blocks;
        self.attacker_slots += other.attacker_slots;
        self.attacker_paid += other.attacker_paid;
        self.attacker_red += other.attacker_red;
    }

    /// The attacker's share of the reward slots paid.
    fn attacker_share(&self) -> f64 {
        self.attacker_paid as f64 / (self.attacker_paid + self.honest_paid) as f64
    }

    /// The share of the honest miners' reward slots that went to red blocks.
    fn honest_lost(&self) -> f64 {
        1.0 - self.honest_paid as f64 / self.honest_slots as f64
    }

    /// The share of the honest miners' reward slots that would go to red blocks if merged empty blocks were paid even
    /// when red (only the red non-empty blocks would be lost).
    fn honest_lost_if_empty_paid(&self) -> f64 {
        self.honest_red_light as f64 / self.honest_slots as f64
    }

    /// The attacker's reward slots paid per unit of its hashrate, relative to mining honestly (1.0).
    fn attacker_yield(&self, alpha: f64) -> f64 {
        self.attacker_paid as f64 / (alpha * self.hours * HOUR)
    }

    /// Blocks per hour: every block's header goes to BTC.
    fn blocks_per_hour(&self) -> f64 {
        (self.honest_blocks + self.attacker_blocks) as f64 / self.hours
    }
}

struct Sim<'a> {
    params: &'a Params,
    works: &'a Works,
    k: usize,
    max_parents: usize,
    load: Load,
    strategy: Strategy,
    alpha: f64,
    dag: BlockDag<bool>,
    /// Published blocks without published children.
    public_tips: BTreeSet<Hash>,
    /// The attacker's unpublished chain, oldest first.
    private: Vec<Hash>,
    /// Honest blocks found since the private chain started (not in its past).
    exposed: usize,
    rng: SplitMix64,
}

impl<'a> Sim<'a> {
    fn new(works: &'a Works, load: Load, strategy: Strategy, alpha: f64, seed: u64) -> Self {
        let params = GhostdagParams::from_params(&works.params);
        let mut rng = SplitMix64(seed);
        let genesis = rng.hash();
        Sim {
            params: &works.params,
            works,
            k: usize::from(params.k),
            max_parents: params.max_block_parents,
            load,
            strategy,
            alpha,
            dag: BlockDag::new(params, genesis, works.of(1), false),
            public_tips: BTreeSet::from([genesis]),
            private: Vec::new(),
            exposed: 0,
            rng,
        }
    }

    /// The best `MAX_BLOCK_PARENTS` of `tips` by the fork-choice key `(chain_work, hash)` (D-100), best first.
    fn best(&self, tips: impl Iterator<Item = Hash>) -> Vec<Hash> {
        let mut keyed: Vec<(U256, Hash)> = tips.map(|t| self.dag.chain_key(&t).unwrap()).collect();
        keyed.sort_unstable_by(|a, b| b.cmp(a));
        keyed.truncate(self.max_parents);
        keyed.into_iter().map(|(_, hash)| hash).collect()
    }

    fn public_parents(&self) -> Vec<Hash> {
        self.best(self.public_tips.iter().copied())
    }

    fn honest_template(&self) -> Template {
        let parents = self.public_parents();
        match self.load {
            Load::Busy => (parents, 1, 0),
            Load::Idle => self.empty_template(parents),
        }
    }

    /// An empty block on `parents`: the streak and the weight follow the selected parent as the consensus computes
    /// them (D-090).
    fn empty_template(&self, parents: Vec<Hash>) -> Template {
        let streak = self.dag.parent_streak(&parents).unwrap() + 1;
        let weight = self.params.empty_block_weight(streak);
        (parents, weight, streak)
    }

    fn attacker_template(&self) -> Template {
        match self.strategy {
            Strategy::Honest => self.honest_template(),
            Strategy::LightPublic => (self.public_parents(), 1, 0),
            Strategy::LightWithhold => match self.private.last() {
                Some(tip) => (vec![*tip], 1, 0),
                None => (self.public_parents(), 1, 0),
            },
            Strategy::HeavyWithhold => match self.private.last() {
                Some(tip) => (vec![*tip], 1, 0),
                None => self.empty_template(self.public_parents()),
            },
        }
    }

    fn mine(&mut self, (parents, weight, streak): Template, attacker: bool) -> Hash {
        let hash = self.rng.hash();
        let work = self.works.of(weight);
        self.dag
            .insert(hash, parents, work, weight, streak, attacker)
            .unwrap();
        hash
    }

    fn publish(&mut self, hash: Hash) {
        for parent in &self.dag.get(&hash).unwrap().parents {
            self.public_tips.remove(parent);
        }
        self.public_tips.insert(hash);
    }

    fn publish_private(&mut self) {
        for hash in std::mem::take(&mut self.private) {
            self.publish(hash);
        }
        self.exposed = 0;
    }

    /// [`Strategy::LightWithhold`] and [`Strategy::HeavyWithhold`]: whether to publish the private chain now.
    fn should_publish(&self) -> bool {
        let Some(tip) = self.private.last() else {
            return false;
        };
        if self.private.len() >= MAX_PRIVATE {
            return true;
        }
        if self.exposed == 0 {
            // no honest block to turn red yet
            return false;
        }
        let mut candidates = self.public_parents();
        candidates.push(*tip);
        let preferred = self.dag.selected_parent(&candidates).unwrap() == *tip;
        if !preferred || self.private.len() > self.k {
            return true;
        }
        // a heavy private block turns the honest blocks found meanwhile red beyond the first K once it is the selected
        // parent: the more there are, the more turn red, so it waits until the next honest block would overtake it
        self.strategy == Strategy::HeavyWithhold && self.next_honest_overtakes(tip)
    }

    /// Whether the honest miners' next block would have at least the chain work of the private tip `tip`.
    fn next_honest_overtakes(&self, tip: &Hash) -> bool {
        let (parents, weight, _) = self.honest_template();
        let data = self.dag.compute(&parents, weight).unwrap();
        let next = data.blue_work.checked_add(self.works.of(weight)).unwrap();
        next >= self.dag.chain_work(tip).unwrap()
    }

    /// Runs for `hours` and returns the rewards.
    fn run(mut self, hours: f64) -> Tally {
        let horizon = hours * HOUR;
        let mut now = 0.0;
        loop {
            let honest = self.honest_template();
            let attacker = self.attacker_template();
            let honest_rate = (1.0 - self.alpha) / honest.1 as f64;
            let attacker_rate = self.alpha / attacker.1 as f64;
            now += self.rng.exponential(honest_rate + attacker_rate);
            if now >= horizon {
                break;
            }
            if self.rng.unit() * (honest_rate + attacker_rate) <= honest_rate {
                let hash = self.mine(honest, false);
                self.publish(hash);
                if !self.private.is_empty() {
                    self.exposed += 1;
                }
            } else {
                let hash = self.mine(attacker, true);
                match self.strategy {
                    Strategy::LightWithhold | Strategy::HeavyWithhold => self.private.push(hash),
                    Strategy::Honest | Strategy::LightPublic => self.publish(hash),
                }
            }
            if self.should_publish() {
                self.publish_private();
            }
        }
        // the attacker hands in what it still holds, and a last block merges every tip
        self.publish_private();
        self.tally(hours)
    }

    fn tally(&self, hours: f64) -> Tally {
        let tips = self.best(self.public_tips.iter().copied());
        assert_eq!(tips.len(), self.public_tips.len(), "too many tips to merge");
        let last = self.dag.compute(&tips, 1).unwrap();
        let mut tally = Tally {
            hours,
            ..Tally::default()
        };
        let mut counted = 0;
        let mut count = |hash: &Hash, rewarded: bool| {
            let weight = self.dag.weight(hash).unwrap();
            let attacker = *self.dag.payload(hash).unwrap();
            counted += 1;
            if attacker {
                tally.attacker_blocks += 1;
                tally.attacker_slots += weight;
                if rewarded {
                    tally.attacker_paid += weight;
                } else {
                    tally.attacker_red += 1;
                }
            } else {
                tally.honest_blocks += 1;
                tally.honest_slots += weight;
                if rewarded {
                    tally.honest_paid += weight;
                } else {
                    tally.honest_red += 1;
                    if weight >= 96 {
                        tally.honest_heavy_red += 1;
                    }
                    if weight == 1 {
                        tally.honest_red_light += 1;
                    }
                }
            }
        };
        // along the selected chain every block is a chain block or in exactly one chain block's merge set
        let mut data = &last;
        loop {
            for blue in data.mergeset_blues.iter().skip(1) {
                count(blue, true);
            }
            for red in &data.mergeset_reds {
                count(red, false);
            }
            let Some(sp) = data.selected_parent else {
                break;
            };
            data = self.dag.data(&sp).unwrap();
            if data.selected_parent.is_none() {
                // the genesis block
                break;
            }
            count(&sp, true);
        }
        assert_eq!(counted, self.dag.len() - 1, "every block counted once");
        tally
    }
}

/// `runs` independent runs of `hours` each, summed.
fn simulate(
    works: &Works,
    load: Load,
    strategy: Strategy,
    alpha: f64,
    runs: u64,
    hours: f64,
) -> Tally {
    let mut total = Tally::default();
    for run in 0..runs {
        total.add(&Sim::new(works, load, strategy, alpha, 0x5e1f + run).run(hours));
    }
    total
}

fn format_row(label: &str, alpha: f64, t: &Tally) -> String {
    format!(
        "{label:<28} a={alpha:<4} share {:>6.2}% yield {:>5.3} honest lost {:>6.2}% (red empty paid {:>5.2}%) red honest {:>5} (heavy {:>4}) red attacker {:>4} blocks/h {:>6.1}",
        100.0 * t.attacker_share(),
        t.attacker_yield(alpha),
        100.0 * t.honest_lost(),
        100.0 * t.honest_lost_if_empty_paid(),
        t.honest_red,
        t.honest_heavy_red,
        t.attacker_red,
        t.blocks_per_hour(),
    )
}

/// Mining like the honest miners, or publishing weight-1 blocks at once, pays each side its hashrate share and turns
/// nothing red; weight-1 blocks published at once keep the honest empty-block streak short, so an idle chain carries
/// more than ten times as many headers.
#[test]
fn publishing_at_once_pays_each_side_its_share() {
    let works = Works::new(&MAINNET);
    let alpha = 0.2;
    let idle = simulate(&works, Load::Idle, Strategy::Honest, alpha, 10, 24.0);
    let light = simulate(&works, Load::Idle, Strategy::LightPublic, alpha, 10, 24.0);
    let busy = simulate(&works, Load::Busy, Strategy::Honest, alpha, 2, 6.0);
    for (label, t) in [
        ("idle/honest", &idle),
        ("idle/light public", &light),
        ("busy/honest", &busy),
    ] {
        println!("{}", format_row(label, alpha, t));
        assert_eq!(t.honest_red + t.attacker_red, 0, "{label}");
    }
    // an idle chain has few blocks, most of them heavy, hence the wider margin
    assert!((idle.attacker_share() - alpha).abs() < 0.05, "{idle:?}");
    assert!((light.attacker_share() - alpha).abs() < 0.015, "{light:?}");
    assert!((busy.attacker_share() - alpha).abs() < 0.015, "{busy:?}");
    assert!(
        light.blocks_per_hour() > 10.0 * idle.blocks_per_hour(),
        "{light:?} vs {idle:?}"
    );
}

/// Idle chain (D-100): an honest heavy empty block counts in the fork choice as soon as it exists, so a private chain of
/// weight-1 blocks is almost never preferred over it: withholding them turns next to nothing red and costs the attacker
/// the blocks merged too late to be blue. Before D-100 a 5% attacker turned 30% of the honest reward slots red and a 10%
/// one 45% (D-098).
#[test]
fn withholding_light_blocks_no_longer_pays_on_an_idle_chain() {
    let works = Works::new(&MAINNET);
    for alpha in [0.05, 0.1] {
        let t = simulate(&works, Load::Idle, Strategy::LightWithhold, alpha, 10, 24.0);
        println!("{}", format_row("idle/light withhold", alpha, &t));
        assert!(t.honest_lost() < 0.01, "{t:?}");
        assert_eq!(t.honest_heavy_red, 0, "{t:?}");
        assert!(t.attacker_share() < alpha, "{t:?}");
        assert!(t.attacker_yield(alpha) < 0.9, "{t:?}");
    }
}

/// What D-100 leaves, with `GHOSTDAG_K` 16: an attacker that finds an empty block keeps it private, extends it with
/// weight-1 blocks and publishes just before the honest blocks found meanwhile would overtake it. On a busy chain its
/// empty block is worth twelve honest blocks, fewer than K + 1, so those stay blue and a 10% attacker takes next to
/// nothing; on an idle chain the honest blocks are empty blocks minutes apart, and the private chain can still put more
/// than K blocks in an honest block's anticone, so it takes a few percent of the honest reward slots. With D-063's K = 8 a
/// 10% attacker took 3.6% of the honest slots on a busy chain and 5.4% on an idle one (D-100).
#[test]
fn withholding_a_heavy_block_takes_little_with_k_16() {
    let works = Works::new(&MAINNET);
    let alpha = 0.1;
    let busy = simulate(&works, Load::Busy, Strategy::HeavyWithhold, alpha, 2, 6.0);
    let idle = simulate(&works, Load::Idle, Strategy::HeavyWithhold, alpha, 10, 24.0);
    println!("{}", format_row("busy/heavy withhold", alpha, &busy));
    println!("{}", format_row("idle/heavy withhold", alpha, &idle));
    assert!(busy.honest_lost() < 0.005, "{busy:?}");
    assert_eq!(busy.attacker_red, 0, "{busy:?}");
    assert!(
        idle.honest_lost() > 0.01 && idle.honest_lost() < 0.06,
        "{idle:?}"
    );
}

/// Busy chain: withholding does not pay. Hardly anything turns red, and the attacker's share of the slots paid stays at
/// its hashrate share.
#[test]
fn withholding_does_not_pay_on_a_busy_chain() {
    let works = Works::new(&MAINNET);
    for alpha in [0.1, 0.2] {
        let t = simulate(&works, Load::Busy, Strategy::LightWithhold, alpha, 2, 6.0);
        println!("{}", format_row("busy/light withhold", alpha, &t));
        assert!(t.honest_lost() < 0.01, "{t:?}");
        assert!(t.attacker_share() < alpha + 0.015, "{t:?}");
        assert!(t.attacker_yield(alpha) < 1.02, "{t:?}");
    }
}

/// The full tables, which SPEC §17.4 and D-100 summarize (D-098 records the runs before D-100):
/// `cargo test -p ksana-consensus --release --test selfish_mining -- --ignored --nocapture` (`KSANA_SM_HOURS` sets the
/// simulated hours per row, 2 400 by default: about four minutes). `KSANA_SM_K` and `KSANA_SM_N` replace `GHOSTDAG_K`
/// and `EMPTY_BLOCK_DIFFICULTY` for a what-if: the lever against withholding a heavy block (D-100).
#[test]
#[ignore = "prints the full table; slow"]
fn print_selfish_mining_table() {
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    let hours: f64 = std::env::var("KSANA_SM_HOURS")
        .ok()
        .and_then(|h| h.parse().ok())
        .unwrap_or(2_400.0);
    let params = Params {
        ghostdag_k: var("KSANA_SM_K").map_or(MAINNET.ghostdag_k, |k| u16::try_from(k).unwrap()),
        empty_block_difficulty: var("KSANA_SM_N").unwrap_or(MAINNET.empty_block_difficulty),
        ..MAINNET
    };
    println!(
        "GHOSTDAG_K {}, EMPTY_BLOCK_DIFFICULTY {}, {hours} hours per row",
        params.ghostdag_k, params.empty_block_difficulty
    );
    let works = Works::new(&params);
    for load in [Load::Idle, Load::Busy] {
        for strategy in [
            Strategy::Honest,
            Strategy::LightPublic,
            Strategy::LightWithhold,
            Strategy::HeavyWithhold,
        ] {
            for alpha in [0.05, 0.1, 0.2, 0.3] {
                let runs = 10;
                let t = simulate(&works, load, strategy, alpha, runs, hours / runs as f64);
                println!(
                    "{}",
                    format_row(&format!("{load:?}/{strategy:?}"), alpha, &t)
                );
            }
        }
    }
}
