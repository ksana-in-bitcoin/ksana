//! Measures how the in-memory structures of the node and the indexer grow with the length of the fast-lane chain, and
//! how long the operations that walk the whole history take (M25, D-097).
//!
//! Each structure is built in a child process of its own for `--blocks` blocks of the chosen shape, and the resident
//! memory it adds is divided by the number of blocks (read from `/proc/self/status`, so the figures are Linux only;
//! they include the allocator's own overhead, which the process pays as well). The per-block figures are then
//! extrapolated to a year of mainnet: an active year is 6 307 200 blocks (one block of weight 1 per
//! `TARGET_BLOCK_TIME` = 5 s), an idle year 52 560 blocks (one empty block of weight 120 per 600 s, D-090).
//!
//! Shapes: `active` = non-empty blocks, with two sibling blocks every `--parallel-every` rounds that the next block
//! merges (a merge set of two); `idle` = one chain of empty blocks (weight 12 → 120 by streak).
//!
//! ```text
//! cargo run --release -p ksana-node --example memory_profile -- --blocks 200000 --shape active
//! cargo run --release -p ksana-node --example memory_profile -- --blocks 50000 --shape idle
//! cargo run --release -p ksana-node --example memory_profile -- --blocks 100000 --shape active --prune
//! ```
//!
//! The non-DAG structures (main-chain summaries, the BTC snapshot's counted headers and landed bodies, the finality
//! state, the indexer's published roots) are built with the same Rust types and the entry counts of the chosen shape,
//! without running the BTC tracker: their size depends only on the types and the counts. The landed bodies are kept as
//! the transactions that landed them, one shared copy per transaction (D-135); here one transaction per BTC block,
//! holding the Borsh encoding of that block's bodies (more than the compact encoding a payload uses).
//!
//! With `--prune` the two DAGs are pruned while they grow, the way the node and the indexer prune (D-097): the pruning
//! root is the main-chain block `--lag` blue score below the tip (by default how far the root trails the tip in this
//! shape: `BTC_FINALITY_CONFS` + `PRUNE_BTC_MARGIN` + 1 BTC blocks of fast-lane blocks), the pruning point
//! `min_pruning_depth` below the root, advanced in steps of `PRUNE_STEP`; the resident memory is sampled at every
//! tenth of the run. Pruning keeps every other structure to the same window of blocks (the pruning point and its
//! future, D-097), so each is measured at the window's size: `lag + min_pruning_depth + PRUNE_STEP` blocks.

// A measurement tool, not consensus code: timing uses the system clock.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ksana_btc::finality::Decision;
use ksana_btc::headers::{HeaderDag, Insert};
use ksana_btc::prune::{PRUNE_BTC_MARGIN, PRUNE_STEP};
use ksana_btc::tracker::{
    BlockSummary, BtcSnapshot, Counted, LandedBody, Landing, MainEntry, Sender,
};
use ksana_consensus::{
    empty_entries_root, genesis_header, BlockDag, Consensus, GhostdagParams, Hash, PowConsensus,
    Sha256d, U256,
};
use ksana_node::chain::BlockInfo;
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, BlockHeader, BtcAnchor, Entry, FeeMode, SigScheme, VmOp, DEVNET, KSA_ASSET_ID,
    VMOP_VERSION,
};

/// Blocks in a fully active year: one block of weight 1 every 5 seconds.
const ACTIVE_YEAR_BLOCKS: u64 = 365 * 24 * 3600 / 5;
/// Blocks in an idle year: one empty block of weight 120 every 600 seconds.
const IDLE_YEAR_BLOCKS: u64 = 365 * 24 * 3600 / 600;
/// Fast-lane blocks per BTC block in an active period (600 s / 5 s).
const ACTIVE_BLOCKS_PER_BTC_BLOCK: u64 = 120;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Active { parallel_every: u64 },
    Idle,
}

struct Args {
    blocks: u64,
    shape: Shape,
    only: Option<String>,
    /// Pruning while growing (D-097): how far the pruning root trails the tip, in blue score.
    prune: Option<u64>,
}

/// How far the pruning root trails the tip in the shape (blue score): the root is decided final
/// `BTC_FINALITY_CONFS` BTC blocks after its data lands (about one BTC block after it is produced) and used once it is
/// `PRUNE_BTC_MARGIN` BTC blocks deep.
fn default_lag(shape: Shape) -> u64 {
    let btc_blocks = u64::from(DEVNET.btc_finality_confs) + u64::from(PRUNE_BTC_MARGIN) + 1;
    match shape {
        Shape::Active { .. } => btc_blocks * ACTIVE_BLOCKS_PER_BTC_BLOCK,
        Shape::Idle => btc_blocks,
    }
}

/// The number of blocks pruning keeps: the root's lag behind the tip, the pruning depth below the root, and up to one
/// step the pruning point has not advanced yet.
fn window(lag: u64) -> u64 {
    lag + GhostdagParams::from_params(&DEVNET).min_pruning_depth() + PRUNE_STEP
}

fn parse_args() -> Args {
    let mut blocks = 100_000;
    let mut shape = "active".to_owned();
    let mut parallel_every = 4;
    let mut only = None;
    let mut prune = false;
    let mut lag = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().expect("missing value");
        match arg.as_str() {
            "--blocks" => blocks = value().parse().expect("--blocks: a number"),
            "--shape" => shape = value(),
            "--parallel-every" => {
                parallel_every = value().parse().expect("--parallel-every: a number")
            }
            "--only" => only = Some(value()),
            "--prune" => prune = true,
            "--lag" => lag = Some(value().parse().expect("--lag: a number")),
            other => panic!("unknown argument {other}"),
        }
    }
    let shape = match shape.as_str() {
        "active" => Shape::Active {
            parallel_every: parallel_every.max(2),
        },
        "idle" => Shape::Idle,
        other => panic!("unknown shape {other} (active | idle)"),
    };
    Args {
        blocks,
        shape,
        only,
        prune: prune.then(|| lag.unwrap_or_else(|| default_lag(shape))),
    }
}

/// Resident memory of this process in bytes (`VmRSS` of `/proc/self/status`); `None` off Linux.
fn rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn hash_of(tag: &str, i: u64) -> Hash {
    sha256(format!("{tag}/{i}").as_bytes())
}

/// The counts of a chain of `blocks` blocks of the shape: (main-chain blocks, merged blocks, non-empty blocks, BTC
/// blocks the data lands in).
fn counts(blocks: u64, shape: Shape) -> (u64, u64, u64, u64) {
    match shape {
        // every `parallel_every` rounds one round adds two siblings: per `parallel_every` main blocks one extra
        // block is merged
        Shape::Active { parallel_every } => {
            let merged = blocks / (parallel_every + 1);
            (
                blocks - merged,
                merged,
                blocks,
                blocks.div_ceil(ACTIVE_BLOCKS_PER_BTC_BLOCK),
            )
        }
        Shape::Idle => (blocks, 0, 0, blocks),
    }
}

/// The insertion time and the resident memory at the end of each tenth of a run of [`grow`].
struct Growth {
    tenths: Vec<Duration>,
    rss: Vec<Option<u64>>,
}

impl Growth {
    /// The insertion time of each tenth, in seconds.
    fn time_line(&self) -> String {
        self.tenths
            .iter()
            .map(|t| format!("{:.1}", t.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The resident memory added since `before` at the end of each tenth, in MB.
    fn rss_line(&self, before: Option<u64>) -> String {
        self.rss
            .iter()
            .map(|r| match (r, before) {
                #[allow(clippy::cast_precision_loss)]
                (Some(r), Some(b)) => format!("{:.0}", r.saturating_sub(b) as f64 / 1e6),
                _ => "n/a".to_owned(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Builds the DAG of the shape block by block: `insert(parents, empty, tag)` mines and inserts one block and returns
/// its hash. Every round the new block takes all current tips as parents; every `parallel_every` rounds two sibling
/// blocks are added on the same parents.
fn grow(
    blocks: u64,
    shape: Shape,
    genesis: Hash,
    mut insert: impl FnMut(&[Hash], bool, u64) -> Hash,
) -> Growth {
    let mut tips = vec![genesis];
    let mut made = 0u64;
    let mut round = 0u64;
    // the insertion time of each tenth of the run, and the resident memory at its end
    let mut tenths = vec![Duration::ZERO; 10];
    let mut rss_at = vec![None; 10];
    let empty = shape == Shape::Idle;
    while made < blocks {
        let started = Instant::now();
        let siblings = match shape {
            Shape::Active { parallel_every } if round % parallel_every == parallel_every - 1 => 2,
            _ => 1,
        };
        let parents = std::mem::take(&mut tips);
        for s in 0..siblings.min(blocks - made) {
            tips.push(insert(&parents, empty, made * 2 + s));
            made += 1;
        }
        let tenth = usize::try_from((made.saturating_sub(1)) * 10 / blocks).unwrap_or(9);
        tenths[tenth.min(9)] += started.elapsed();
        rss_at[tenth.min(9)] = rss();
        round += 1;
    }
    Growth {
        tenths,
        rss: rss_at,
    }
}

/// Pruning is looked at once per this many blocks (a node looks once per BTC sync), which spreads the walk down to the
/// pruning root (counted in the insertion time) over as many blocks.
const PRUNE_CHECK_EVERY: u64 = 100;

/// Where pruning goes next when the tip is `tip` (D-097): the pruning root `lag` blue score below the tip on its
/// selected chain and the pruning point `min_pruning_depth` below the root, once the point can advance by
/// `PRUNE_STEP`; `None` otherwise, and on all but one in [`PRUNE_CHECK_EVERY`] blocks (`tag` is the block's tag from
/// [`grow`], half of which is the number of blocks made before it).
fn next_pruning<T>(dag: &BlockDag<T>, tip: &Hash, lag: u64, tag: u64) -> Option<(Hash, Hash)> {
    if !(tag / 2).is_multiple_of(PRUNE_CHECK_EVERY) {
        return None;
    }
    let (_, root) = dag.chain_floor(tip, lag).ok()?;
    let (_, point) = dag
        .chain_floor(&root, dag.params().min_pruning_depth())
        .ok()?;
    let score = |hash: &Hash| dag.data(hash).map(|d| d.blue_score);
    let current = match dag.pruning_point() {
        Some(current) => score(current)?,
        None => 0,
    };
    (score(&point)? >= current + PRUNE_STEP).then_some((root, point))
}

/// A header on `parents` (mined with the test algorithm at the devnet PoW limit, on the ASERT schedule so the target
/// stays at the limit, `genesis_time` being the genesis block's timestamp), with its weight, streak and work.
fn mine(
    consensus: &PowConsensus,
    dag: &BlockDag<impl Sized>,
    header_of: impl Fn(&Hash) -> BlockHeader,
    genesis_time: u64,
    parents: &[Hash],
    empty: bool,
    tag: u64,
) -> (BlockHeader, u64, u64, U256) {
    let sp = dag.selected_parent(parents).expect("selected parent");
    let sp_header = header_of(&sp);
    let sp_streak = dag.streak(&sp).expect("streak");
    let mut rest: Vec<Hash> = parents.iter().copied().filter(|p| *p != sp).collect();
    rest.sort_unstable();
    let mut header = BlockHeader {
        version: 0,
        height: sp_header.height + 1,
        parents: [vec![sp], rest].concat(),
        btc_anchor: sp_header.btc_anchor.clone(),
        entries_root: if empty {
            empty_entries_root()
        } else {
            hash_of("entries", tag)
        },
        state_root: hash_of("state", tag),
        producer: hash_of("producer", tag % 16),
        timestamp: 0,
        bits: 0,
        nonce: 0,
    };
    let weight = consensus.weight(&header, sp_streak);
    let streak = consensus.streak(&header, sp_streak);
    let data = dag.compute(parents, weight).expect("GHOSTDAG");
    header.version = consensus.params().protocol_version(data.daa_score);
    // an hour behind the ASERT schedule: the target stays at the PoW limit
    header.timestamp = genesis_time + 5 * data.daa_score + 3600;
    header.bits = consensus
        .next_bits(&sp_header, data.daa_score, weight)
        .expect("bits");
    if empty {
        // one BTC block per idle block
        header.btc_anchor = BtcAnchor {
            height: sp_header.btc_anchor.height + 1,
            hash: hash_of("btc", u64::from(sp_header.btc_anchor.height) + 1),
        };
    }
    assert!(consensus
        .solve(&mut header, weight, 0, 1 << 20)
        .expect("solve"));
    let work = consensus.work(&header, weight).expect("work");
    (header, weight, streak, work)
}

fn setup() -> (PowConsensus, BlockHeader) {
    let genesis = genesis_header(
        &DEVNET,
        1_790_000_000,
        BtcAnchor {
            height: 100,
            hash: hash_of("btc", 100),
        },
        [1; 32],
    )
    .expect("genesis");
    let consensus =
        PowConsensus::with_algorithm(&DEVNET, &genesis, Arc::new(Sha256d)).expect("consensus");
    (consensus, genesis)
}

fn transfer_entry(i: u64) -> Entry {
    Entry::Sponsored {
        op: VmOp {
            version: VMOP_VERSION,
            chain_id: DEVNET.chain_id,
            account: hash_of("account", i % 1000),
            nonce: i,
            fee: FeeMode::Token { amount: 1 },
            max_gas: 100_000,
            action: Action::AssetTransfer {
                asset: KSA_ASSET_ID,
                to: hash_of("to", i),
                amount: 5,
            },
            sig: SigScheme::Bip340([7; 64]),
        },
    }
}

/// A transaction holding `len` bytes of data in its witness, standing for the one that landed some bodies (D-135).
fn body_tx(len: usize) -> bitcoin::Transaction {
    bitcoin::Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::null(),
            script_sig: bitcoin::ScriptBuf::new(),
            sequence: bitcoin::Sequence::MAX,
            witness: bitcoin::Witness::from_slice(&[vec![0u8; len]]),
        }],
        output: Vec::new(),
    }
}

fn landing(i: u64, height: u32) -> Landing {
    Landing {
        txid: hash_of("txid", i),
        height,
        index: 1,
        sender: Sender::Block(hash_of("sender", i)),
    }
}

/// The main-chain summaries of the shape (the node's `main` and the indexer's executed main chain).
fn main_entries(blocks: u64, shape: Shape) -> Vec<MainEntry> {
    let (main, merged, _, _) = counts(blocks, shape);
    let every = main.checked_div(merged).unwrap_or(u64::MAX);
    (0..main)
        .map(|i| MainEntry {
            hash: hash_of("main", i),
            settles: shape != Shape::Idle,
            anchor_height: u32::try_from(i / 120).unwrap_or(u32::MAX),
            producer: hash_of("producer", i % 16),
            daa_score: i,
            weight: 1,
            merged: if i % every == every - 1 {
                vec![BlockSummary {
                    hash: hash_of("merged", i),
                    settles: true,
                    anchor_height: 0,
                    producer: hash_of("producer", i % 16),
                    daa_score: i,
                    weight: 1,
                    rewarded: true,
                }]
            } else {
                Vec::new()
            },
        })
        .collect()
}

/// One structure built for `blocks` blocks of `shape` (the DAGs pruned while growing when `prune` gives the pruning
/// root's lag); returns it (kept alive while the memory is read) and any timing lines to report.
fn build(
    name: &str,
    blocks: u64,
    shape: Shape,
    prune: Option<u64>,
) -> (Box<dyn std::any::Any>, Vec<String>) {
    let (main, merged, non_empty, btc_blocks) = counts(blocks, shape);
    let total = main + merged;
    let mut notes = Vec::new();
    let before = rss();
    let built: Box<dyn std::any::Any> = match name {
        "header-dag" => {
            // the header DAG of the finality decision (node) and of the indexer: GHOSTDAG data + headers + main chain
            let (consensus, genesis) = setup();
            let mut tree =
                HeaderDag::new(&DEVNET, &consensus, genesis.clone()).expect("header DAG");
            // no BTC chain: no timestamp bound is checked and no anchor is off it
            let btc = BtcSnapshot::default();
            let growth = grow(blocks, shape, genesis.hash(), |parents, empty, tag| {
                let (header, _, _, _) = mine(
                    &consensus,
                    tree.dag(),
                    |h| tree.header(h).cloned().expect("header"),
                    genesis.timestamp,
                    parents,
                    empty,
                    tag,
                );
                let hash = header.hash();
                let outcome = tree.insert(&consensus, header, &btc);
                assert_eq!(outcome, Insert::Connected(1));
                if let Some((root, point)) =
                    prune.and_then(|lag| next_pruning(tree.dag(), &hash, lag, tag))
                {
                    tree.set_floor(root, &btc).expect("finality floor");
                    tree.prune(&point).expect("prune");
                }
                hash
            });
            notes.push(format!(
                "header DAG: mining + insertion time, first tenth {:?}, last tenth {:?}",
                growth.tenths[0], growth.tenths[9]
            ));
            if prune.is_some() {
                notes.push(format!(
                    "header DAG with pruning: {} blocks held at the end; mining + insertion + pruning time of each \
                     tenth (s): {}; resident memory added at the end of each tenth (MB): {}",
                    tree.dag().len(),
                    growth.time_line(),
                    growth.rss_line(before)
                ));
            }
            notes.push(format!(
                "header DAG: {} selected-chain tips at the end (main-chain selection visits only those heavier than the main-chain tip)",
                tree.dag().selected_chain_tips().len()
            ));
            Box::new(tree)
        }
        "block-dag" => {
            // the node's block DAG: GHOSTDAG data + BlockInfo
            let (consensus, genesis) = setup();
            let genesis_hash = genesis.hash();
            let mut dag = BlockDag::new(
                GhostdagParams::from_params(&DEVNET),
                genesis_hash,
                consensus.genesis_work(&genesis).expect("work"),
                BlockInfo {
                    header: genesis.clone(),
                    settles: false,
                    carries: false,
                },
            );
            let mut tip = genesis_hash;
            let growth = grow(blocks, shape, genesis_hash, |parents, empty, tag| {
                let (header, weight, streak, work) = mine(
                    &consensus,
                    &dag,
                    |h| dag.payload(h).expect("block").header.clone(),
                    genesis.timestamp,
                    parents,
                    empty,
                    tag,
                );
                let hash = header.hash();
                dag.insert(
                    hash,
                    header.parents.clone(),
                    work,
                    weight,
                    streak,
                    BlockInfo {
                        header,
                        settles: !empty,
                        carries: false,
                    },
                )
                .expect("insert");
                if let Some((_, point)) = prune.and_then(|lag| next_pruning(&dag, &hash, lag, tag))
                {
                    dag.prune(&point).expect("prune");
                }
                tip = hash;
                hash
            });
            if prune.is_some() {
                notes.push(format!(
                    "node block DAG with pruning: {} blocks held at the end; mining + insertion + pruning time of each \
                     tenth (s): {}; resident memory added at the end of each tenth (MB): {}",
                    dag.len(),
                    growth.time_line(),
                    growth.rss_line(before)
                ));
                return (Box::new(dag), notes);
            }
            // a side block on the genesis block with almost no work: in_past from the tip walks the whole chain
            let side = hash_of("side", 0);
            dag.insert(
                side,
                vec![genesis_hash],
                U256::ONE,
                1,
                0,
                BlockInfo {
                    header: genesis.clone(),
                    settles: false,
                    carries: false,
                },
            )
            .expect("side block");
            let started = Instant::now();
            let in_past = dag.in_past(&side, &tip).expect("in_past");
            let walk = started.elapsed();
            let started = Instant::now();
            let merge = dag.compute(&[tip, side], 1).map(|_| ());
            let compute = started.elapsed();
            notes.push(format!(
                "block DAG: in_past(low-work side block on genesis, tip) = {in_past} took {walk:?}; merging it: {merge:?} after {compute:?}"
            ));
            Box::new(dag)
        }
        "main-entries" => {
            let entries = main_entries(blocks, shape);
            let started = Instant::now();
            let copy = entries.clone();
            notes.push(format!(
                "main-chain summaries: one clone (done on every connected block) takes {:?}",
                started.elapsed()
            ));
            drop(copy);
            Box::new(entries)
        }
        "accepted" => {
            // the node's accepted map: every accepted block → its main-chain height
            let map: BTreeMap<Hash, u64> = (0..total).map(|i| (hash_of("main", i), i)).collect();
            Box::new(map)
        }
        "snapshot" => {
            // the BTC snapshot's per-block maps (D-135): counted, headers_by_height, bodies, ksn1_txs
            let per_btc = total.div_ceil(btc_blocks.max(1)).max(1);
            let height_of = |i: u64| u32::try_from(i / per_btc).unwrap_or(u32::MAX);
            let counted: BTreeMap<[u8; 32], Counted> = (0..total)
                .map(|i| {
                    let height = height_of(i);
                    let counted = Counted {
                        landing: landing(i, height),
                        anchor: height.saturating_sub(1),
                        parent_anchor: height.saturating_sub(1),
                        settles: i < non_empty,
                    };
                    (hash_of("main", i), counted)
                })
                .collect();
            let mut headers_by_height: BTreeMap<u32, Vec<[u8; 32]>> = BTreeMap::new();
            for i in 0..total {
                headers_by_height
                    .entry(height_of(i))
                    .or_default()
                    .push(hash_of("main", i));
            }
            // one transaction per BTC block lands the bodies of that block's non-empty blocks
            let mut bodies: BTreeMap<[u8; 32], LandedBody> = BTreeMap::new();
            let mut i = 0;
            while i < non_empty {
                let height = height_of(i);
                let end =
                    non_empty.min(u64::from(height).saturating_add(1).saturating_mul(per_btc));
                let len = (i..end)
                    .map(|j| borsh::to_vec(&transfer_entry(j)).map_or(0, |b| b.len()))
                    .sum();
                let tx = Arc::new(body_tx(len));
                for (slot, j) in (i..end).enumerate() {
                    let body = LandedBody {
                        landing: landing(j, height),
                        tx: Arc::clone(&tx),
                        slot: u32::try_from(slot).unwrap_or(u32::MAX),
                    };
                    bodies.insert(hash_of("main", j), body);
                }
                i = end.max(i.saturating_add(1));
            }
            let ksn1_txs: BTreeMap<[u8; 32], u32> = (0..btc_blocks)
                .map(|i| (hash_of("txid", i), u32::try_from(i).unwrap_or(u32::MAX)))
                .collect();
            let started = Instant::now();
            let copy = (
                counted.clone(),
                headers_by_height.clone(),
                bodies.clone(),
                ksn1_txs.clone(),
            );
            notes.push(format!(
                "BTC snapshot: one copy (done on every BTC sync that changes it; the transactions are shared) takes {:?}",
                started.elapsed()
            ));
            drop(copy);
            // the settled-prefix style scan: every main-chain block looked up in the counted map, from height 1
            let entries = main_entries(blocks, shape);
            let started = Instant::now();
            let mut landed = 0u64;
            for entry in &entries {
                for summary in entry.accepted() {
                    if counted.contains_key(&summary.hash) {
                        landed += 1;
                    }
                }
            }
            notes.push(format!(
                "settled prefix: one scan of the main chain against the counted headers ({landed} found) takes {:?}",
                started.elapsed()
            ));
            Box::new((counted, headers_by_height, bodies, ksn1_txs))
        }
        "finality" => {
            // the finality state: decisions, conn, closure_landed, evaluated
            let decisions: Vec<Decision> = (0..main)
                .map(|i| Decision {
                    hash: hash_of("main", i),
                    height: i + 1,
                    btc_height: u32::try_from(i / 120).unwrap_or(u32::MAX),
                    btc_hash: hash_of("btc", i / 120),
                    landed: u32::try_from(i / 120).unwrap_or(u32::MAX),
                })
                .collect();
            let conn: BTreeMap<Hash, u32> = (0..total)
                .map(|i| (hash_of("main", i), u32::try_from(i).unwrap_or(u32::MAX)))
                .collect();
            let closure_landed: BTreeMap<Hash, u32> = (0..total)
                .map(|i| (hash_of("main", i), u32::try_from(i).unwrap_or(u32::MAX)))
                .collect();
            let evaluated: Vec<(u32, [u8; 32])> = (0..btc_blocks)
                .map(|i| (u32::try_from(i).unwrap_or(u32::MAX), hash_of("btc", i)))
                .collect();
            Box::new((decisions, conn, closure_landed, evaluated))
        }
        "indexer-roots" => {
            // the indexer's published roots; the bodies it executes are decoded from the snapshot's landed
            // transactions when used (D-135)
            let published: Vec<([u8; 32], [u8; 32])> = (0..main)
                .map(|i| (hash_of("main", i), hash_of("state", i)))
                .collect();
            Box::new(published)
        }
        "selected-tips" => {
            // the selected-chain tips a node keeps: every merged block stays one (scanned on every main-chain choice)
            let tips: BTreeSet<Hash> = (0..merged).map(|i| hash_of("merged", i)).collect();
            Box::new(tips)
        }
        other => panic!("unknown structure {other}"),
    };
    (built, notes)
}

const STRUCTURES: [(&str, &str); 8] = [
    ("header-dag", "header DAG (finality; indexer)"),
    ("block-dag", "node block DAG (GHOSTDAG + BlockInfo)"),
    ("main-entries", "main-chain summaries (node; indexer)"),
    ("accepted", "node accepted map"),
    (
        "snapshot",
        "BTC snapshot: counted headers and landed bodies",
    ),
    ("finality", "finality state"),
    ("indexer-roots", "indexer published roots"),
    ("selected-tips", "selected-chain tips (merged blocks)"),
];

fn main() {
    let args = parse_args();
    if let Some(name) = args.only {
        // child: build one structure and report the resident memory it added
        let before = rss();
        let (built, notes) = build(&name, args.blocks, args.shape, args.prune);
        let after = rss();
        std::hint::black_box(&built);
        let added = match (before, after) {
            (Some(b), Some(a)) => a.saturating_sub(b).to_string(),
            _ => "n/a".to_owned(),
        };
        println!("RESULT {name} {added}");
        for note in notes {
            println!("NOTE {note}");
        }
        return;
    }
    let shape_name = match args.shape {
        Shape::Active { parallel_every } => {
            format!("active, two siblings every {parallel_every} rounds")
        }
        Shape::Idle => "idle, empty blocks only".to_owned(),
    };
    match args.prune {
        Some(lag) => {
            println!(
                "{} blocks, shape: {shape_name}; pruned (D-097): the pruning root {lag} blue score below the tip, the \
                 pruning point min_pruning_depth {} below it, in steps of {PRUNE_STEP}; the other structures measured \
                 at the window of {} blocks",
                args.blocks,
                GhostdagParams::from_params(&DEVNET).min_pruning_depth(),
                window(lag)
            );
            println!("{:<44} {:>14}", "structure", "held (MB)");
        }
        None => {
            println!(
                "{} blocks, shape: {shape_name}; extrapolation: active year {ACTIVE_YEAR_BLOCKS} blocks, idle year {IDLE_YEAR_BLOCKS} blocks",
                args.blocks
            );
            println!(
                "{:<44} {:>12} {:>16} {:>14}",
                "structure", "bytes/block", "active year", "idle year"
            );
        }
    }
    let exe = std::env::current_exe().expect("current exe");
    let mut total_per_block = 0f64;
    let mut held = 0f64;
    let mut notes = Vec::new();
    for (name, label) in STRUCTURES {
        // with pruning the DAGs grow (and are pruned) for the whole run, the rest is measured at the window's size
        let dag = matches!(name, "header-dag" | "block-dag");
        let blocks = match args.prune {
            Some(lag) if !dag => window(lag),
            _ => args.blocks,
        };
        let mut command = Command::new(&exe);
        command.args(["--blocks", &blocks.to_string(), "--only", name]);
        if let (Some(lag), true) = (args.prune, dag) {
            command.args(["--prune", "--lag", &lag.to_string()]);
        }
        match args.shape {
            Shape::Active { parallel_every } => {
                command.args([
                    "--shape",
                    "active",
                    "--parallel-every",
                    &parallel_every.to_string(),
                ]);
            }
            Shape::Idle => {
                command.args(["--shape", "idle"]);
            }
        }
        let output = command.output().expect("child process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            println!(
                "{label:<44} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            continue;
        }
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("RESULT ") {
                let added = rest.split_whitespace().nth(1).unwrap_or("n/a");
                match (added.parse::<f64>(), args.prune) {
                    (Ok(bytes), Some(_)) => {
                        held += bytes;
                        println!("{label:<44} {:>14.1}", bytes / 1e6);
                    }
                    (Ok(bytes), None) => {
                        #[allow(clippy::cast_precision_loss)]
                        let per_block = bytes / args.blocks as f64;
                        total_per_block += per_block;
                        #[allow(clippy::cast_precision_loss)]
                        let (active, idle) = (
                            per_block * ACTIVE_YEAR_BLOCKS as f64,
                            per_block * IDLE_YEAR_BLOCKS as f64,
                        );
                        println!(
                            "{label:<44} {per_block:>12.0} {:>13.2} GB {:>11.1} MB",
                            active / 1e9,
                            idle / 1e6
                        );
                    }
                    (Err(_), _) => println!("{label:<44} {added:>12}"),
                }
            } else if let Some(note) = line.strip_prefix("NOTE ") {
                notes.push(note.to_owned());
            }
        }
    }
    if args.prune.is_some() {
        println!(
            "{:<44} {:>14.1}",
            "total (node and indexer structures summed)",
            held / 1e6
        );
    } else {
        #[allow(clippy::cast_precision_loss)]
        let (active, idle) = (
            total_per_block * ACTIVE_YEAR_BLOCKS as f64,
            total_per_block * IDLE_YEAR_BLOCKS as f64,
        );
        println!(
            "{:<44} {total_per_block:>12.0} {:>13.2} GB {:>11.1} MB",
            "total (node and indexer structures summed)",
            active / 1e9,
            idle / 1e6
        );
    }
    for note in notes {
        println!("- {note}");
    }
}
