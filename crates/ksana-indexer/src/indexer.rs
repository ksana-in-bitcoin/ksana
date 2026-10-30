//! The indexer proper (SPEC §10.3, §10.4; D-038, D-046, D-048, D-049, D-063, D-097, D-135): BTC scanning → the block
//! DAG of the counted headers and the landed bodies → the main chain within the settled closure → block-by-block
//! execution and verification → pruning.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_btc::client::BtcClient;
use ksana_btc::finality::{BtcFinal, Exclusion, Finality};
use ksana_btc::headers::HeaderDagRecord;
use ksana_btc::prune::{read_checkpoint, write_checkpoint, Pruning};
use ksana_btc::tracker::{
    settled_height, unlock_cursor, BtcSnapshot, BtcTracker, KnownBlocks, MainChain, MainEntry,
    SnapshotRecord, TrackerError,
};
use ksana_btc::verdict::{self, Judge, Verdict};
use ksana_consensus::{GhostdagError, PowAlgorithm, PowConsensus};
use ksana_execution::{Executor, GenesisFile};
use ksana_primitives::{BlockHeader, Params};
use ksana_state::{DbMemory, StateDb};

use crate::index::{Index, IndexHistory};
use crate::known::{Known, Plan};
use crate::meta::{Meta, TipRecord};
use crate::IndexerError;

/// Subdirectory of the state store in the data directory.
const STATE_DIR: &str = "state";
/// Subdirectory of the index of the executed main chain (D-097).
const INDEX_DIR: &str = "index";
/// The pruning checkpoint in the data directory (D-097).
const CHECKPOINT_FILE: &str = "pruning.ckpt";
/// The version of the checkpoint's local format (2: the finality decision's exclusions and the basis of each mark of a
/// block unable to land, D-101; 3: no pending headers, unresolved payloads or candidate bodies, the landed bodies kept
/// as their transactions, D-135).
const CHECKPOINT_VERSION: u32 = 3;

/// The BTC tip (height and block hash) a snapshot ends at; `None` before any BTC block is scanned.
type BtcTip = Option<(u32, [u8; 32])>;

/// The indexer's pruning checkpoint (D-097, local format): what a restart continues from instead of scanning BTC from
/// the genesis anchor again. Written at every pruning.
#[derive(BorshSerialize, BorshDeserialize)]
struct Checkpoint {
    /// [`CHECKPOINT_VERSION`].
    version: u32,
    /// The genesis block hash (a checkpoint of another chain is not used).
    genesis: [u8; 32],
    /// The pruning root (hash and height).
    root: ([u8; 32], u64),
    /// The first height of the executed main chain's window.
    main_base: u64,
    /// The settled prefix of the main chain below the window.
    main_base_prefix: u32,
    /// The BTC snapshot.
    snapshot: SnapshotRecord,
    /// The block DAG.
    headers: HeaderDagRecord,
    /// The finality decision.
    finality: Finality,
}

/// What a pruning checkpoint gives back at startup (D-097).
struct Restored {
    known: Known,
    snapshot: BtcSnapshot,
    root: ([u8; 32], u64),
    /// The base and the settled prefix of the executed main chain's window.
    window: (u64, u32),
}

/// Indexer configuration.
#[derive(Clone)]
pub struct IndexerConfig {
    /// Data directory (state store, index, metadata and pruning checkpoint).
    pub data_dir: PathBuf,
    /// Genesis config: must be the same one the fast-lane nodes use (D-027).
    pub genesis: GenesisFile,
    /// Protocol parameters.
    pub params: Params,
    /// bitcoind (requires `-txindex=1`; the mock chain in tests).
    pub btc: Arc<dyn BtcClient>,
    /// Mining algorithm: by default (`None`) the one selected by the network params; tests may swap in another
    /// algorithm (D-062), which must be the same as the nodes'.
    pub pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
    /// Pruning (D-097): on by default ([`Pruning::default`]).
    pub pruning: Pruning,
}

/// Indexer status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexerStatus {
    /// Height of the highest block executed with its `state_root` verified.
    pub height: u64,
    /// Hash of that block. After a restart, before re-verification against the BTC data, all zeros when the metadata
    /// and the state store disagree.
    pub hash: [u8; 32],
    /// State root of that block (equal to the root of the state store's latest version).
    pub state_root: [u8; 32],
    /// Height of the scanned BTC tip; `None` before the first sync with bitcoind.
    pub btc_height: Option<u32>,
    /// Height of the tip of the finality decision (D-082): the floor of the main chain, executed without waiting for
    /// `SETTLEMENT_DEPTH` confirmations; 0 (the genesis block) when there is none yet.
    pub finalized_height: u64,
    /// Hash of the tip of the finality decision.
    pub finalized_hash: [u8; 32],
    /// Height of the BTC-final tip as reported (`finalized` of `indexer_status`, D-114): the highest finalized block
    /// decided at a BTC block that has `BTC_FINALITY_CONFS` confirmations, at or below `finalized_height`.
    pub btc_final_height: u64,
    /// Hash of the BTC-final tip.
    pub btc_final_hash: [u8; 32],
    /// Reason processing halted after a BTC-final block was found invalid or unable to land (including the height and
    /// block hash, D-082); `None` normally.
    pub halted: Option<String>,
}

/// Progress after one sync round ([`Indexer::sync_once`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Highest height executed and verified.
    pub height: u64,
    /// Height of the block tree's main-chain tip (header landed on BTC, but not necessarily within the processable
    /// prefix yet).
    pub main_height: u64,
    /// Height of the scanned BTC tip.
    pub btc_height: Option<u32>,
    /// Height of the tip of the finality decision (D-082; BTC-final as reported is in the status, D-114).
    pub finalized_height: u64,
    /// Whether anything changed this round (BTC snapshot, finality, executed blocks, rollback, pruning).
    pub changed: bool,
    /// How many times this round pruned (D-097); more than once when its BTC scan took several tracker rounds.
    pub prunes: u32,
}

/// State during processing (updated mutually exclusively with BTC scanning).
struct Inner {
    /// The state store; `None` only briefly while rebuilding from genesis.
    state: Option<StateDb>,
    /// The executed main chain held in memory: from the genesis block, or after pruning from its window base up, the
    /// index answering below it (D-097). After startup, before the first verification against the BTC data, only the
    /// genesis block.
    executed: Arc<MainChain>,
    /// Whether verification against the BTC data has happened (the first round after startup verifies by state root,
    /// D-048).
    reconciled: bool,
    /// The verification after startup is waiting for bitcoind to catch up with the state store (logged once).
    lagging: bool,
    /// The previous round did not finish (local fault, stop, or starting over from the genesis anchor): process once
    /// more even when BTC has not changed.
    retry: bool,
    /// Reason processing halted (a BTC-final block is invalid or cannot land, D-082).
    halted: Option<String>,
    /// The block the finality decision is waiting for a verdict on (D-101; logged once when it changes): judged once
    /// the final tip, its selected parent, is executed.
    validating: Option<[u8; 32]>,
    /// The block judging gave no verdict on, and the BTC tip it was judged at: not judged again until the tip changes
    /// (D-101).
    unjudged: Option<([u8; 32], BtcTip)>,
    /// The BTC-final tip as reported (D-114, [`Finality::btc_final`]), computed after every finality decision; `None`
    /// before the first one.
    btc_final: Option<BtcFinal>,
    /// The root of the last pruning (hash and height, D-097); `None` before the first one.
    root: Option<([u8; 32], u64)>,
    /// State versions from this height on are kept even when older than `REORG_WINDOW` (with pruning on, D-097): the
    /// height of the highest executed block decided final at least the pruning margin of BTC blocks ago, the lowest
    /// height a rollback can still go to.
    retain: Option<u64>,
    /// Continuing from a pruning checkpoint: the base and the settled prefix of the executed main chain's window, which
    /// the first verification against the BTC data rebuilds from the index (D-097).
    restored: Option<(u64, u32)>,
}

impl Inner {
    fn height(&self) -> u64 {
        self.executed.tip_height()
    }

    /// The first height of the processable prefix: the genesis block, or the pruning root's after pruning (D-097).
    fn plan_from(&self) -> u64 {
        self.root.map_or(0, |(_, height)| height)
    }
}

/// The open state store (borrows only the `Inner::state` field; the other fields can still be modified at the same
/// time).
fn state_of(slot: &mut Option<StateDb>) -> Result<&mut StateDb, IndexerError> {
    slot.as_mut()
        .ok_or_else(|| IndexerError::Internal("state store not open".into()))
}

/// Published results (read by RPC queries, updated block by block during processing).
struct Published {
    status: IndexerStatus,
    /// Whether the executed blocks have been verified against the BTC data since startup (until then the index is not
    /// consulted for queries).
    verified: bool,
}

/// Failure to execute a block. An invalid block and a block that cannot land are both marked, the main chain is
/// re-selected and processing continues; processing halts only when the block is BTC-final (D-082).
enum Step {
    /// Invalid block: the block or a block it merges violates a state-independent body rule, so the nodes reject it and
    /// every block after it (D-063). It does not halt processing: the indexer picks its main chain from the headers on
    /// BTC alone, so anyone can land a processable block with an invalid body.
    Invalid(String),
    /// The block cannot land (`state_root` mismatch, strict-mode failure, settlement backlog limit, D-063).
    Disqualify(String),
    /// Local fault: retried later, the block is not judged invalid.
    Fault(IndexerError),
}

impl From<ksana_state::StateError> for Step {
    fn from(e: ksana_state::StateError) -> Self {
        Step::Fault(e.into())
    }
}

impl From<IndexerError> for Step {
    fn from(e: IndexerError) -> Self {
        Step::Fault(e)
    }
}

/// How a step of a sync round ended.
enum Round {
    /// Carry on (whether anything changed).
    Go(bool),
    /// Everything was dropped to start over from the genesis anchor (D-097): the round ends, the next one scans BTC
    /// again.
    Rescan,
}

fn internal(message: impl Into<String>) -> IndexerError {
    IndexerError::Internal(message.into())
}

fn poisoned<T>(_: PoisonError<T>) -> IndexerError {
    internal("the indexer's lock is poisoned")
}

/// The genesis block's main-chain entry.
fn genesis_entry(genesis: &BlockHeader) -> MainEntry {
    MainEntry {
        hash: genesis.hash(),
        settles: false,
        anchor_height: genesis.btc_anchor.height,
        producer: genesis.producer,
        daa_score: 0,
        weight: 1,
        merged: Vec::new(),
    }
}

/// The executed main chain holding only the genesis block (answering below its window from the index).
fn genesis_main(genesis: &BlockHeader, index: &Arc<Index>) -> MainChain {
    MainChain::new(genesis_entry(genesis)).with_history(Arc::new(IndexHistory(Arc::clone(index))))
}

/// Deletes the pruning checkpoint (D-097), if any.
fn remove_checkpoint(data_dir: &Path) -> Result<(), IndexerError> {
    let path = data_dir.join(CHECKPOINT_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(IndexerError::Storage(format!(
            "removing {}: {e}",
            path.display()
        ))),
    }
}

/// Reads the pruning checkpoint (D-097) for a state store whose latest version is `latest`. `None` when there is none,
/// or when it cannot be used (with an alert): the indexer then scans BTC from the genesis anchor again.
fn restore(
    data_dir: &Path,
    params: &Params,
    consensus: &PowConsensus,
    genesis: &[u8; 32],
    latest: u64,
) -> Option<Restored> {
    let path = data_dir.join(CHECKPOINT_FILE);
    let checked = read_checkpoint::<Checkpoint>(&path).and_then(|checkpoint| {
        let Some(c) = checkpoint else {
            return Ok(None);
        };
        if c.version != CHECKPOINT_VERSION {
            return Err(format!("unknown version {}", c.version));
        }
        if c.genesis != *genesis {
            return Err("it belongs to another chain".into());
        }
        let (root, root_height) = c.root;
        if root_height > latest {
            return Err(format!(
                "its pruning root at height {root_height} is above the state store's latest version {latest}"
            ));
        }
        if c.main_base > root_height {
            return Err("its main-chain window starts above its pruning root".into());
        }
        if c.finality.root().hash != root {
            return Err("its finality decision does not continue from its pruning root".into());
        }
        let known = Known::restore(
            params.clone(),
            consensus.clone(),
            *genesis,
            (c.headers, c.finality),
        )?;
        let snapshot = BtcSnapshot::from_record(c.snapshot)?;
        Ok(Some(Restored {
            known,
            snapshot,
            root: c.root,
            window: (c.main_base, c.main_base_prefix),
        }))
    });
    match checked {
        Ok(restored) => restored,
        Err(e) => {
            eprintln!(
                "[ksana-indexer] alert: the pruning checkpoint {} cannot be used ({e}); scanning BTC from the genesis anchor \
                 again (D-097)",
                path.display()
            );
            None
        }
    }
}

/// The standalone indexer: see the crate docs. All methods take `&self`, so it can be shared between threads (the RPC
/// server reads the status while a background thread syncs).
pub struct Indexer {
    params: Params,
    data_dir: PathBuf,
    genesis_file: GenesisFile,
    genesis: BlockHeader,
    fingerprint: [u8; 32],
    consensus: PowConsensus,
    exec: Executor,
    tracker: BtcTracker,
    /// The index of the executed main chain (D-097).
    index: Arc<Index>,
    /// The block cache shared by the index and the state store (D-103).
    db_memory: DbMemory,
    /// Pruning settings (D-097).
    pruning: Pruning,
    /// The block tree, candidate and determined bodies. Accessed through [`KnownBlocks`] while the tracker scans, so it
    /// must not be held while calling the tracker.
    known: Mutex<Known>,
    inner: Mutex<Inner>,
    published: RwLock<Published>,
    /// Only one sync round at a time.
    round: Mutex<()>,
    stop: AtomicBool,
}

impl Indexer {
    /// Opens the data directory; on first use initializes the state from the genesis config (same as the nodes: the
    /// genesis state is committed as version 0, D-027). The genesis anchor block must be on bitcoind's best chain
    /// (D-035). Continues from the pruning checkpoint when there is one (D-097). Does not scan BTC: scanning and
    /// execution happen in [`Indexer::sync_once`].
    pub fn open(config: IndexerConfig) -> Result<Indexer, IndexerError> {
        let IndexerConfig {
            data_dir,
            genesis: genesis_file,
            params,
            btc,
            pow_algorithm,
            pruning,
        } = config;
        params
            .check_upgrades()
            .map_err(|e| IndexerError::Config(format!("invalid upgrade table (D-076): {e}")))?;
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| IndexerError::Storage(format!("creating {}: {e}", data_dir.display())))?;
        let fingerprint = genesis_file.fingerprint()?;
        let meta = Meta::load(&data_dir)?;
        if meta.as_ref().is_some_and(|m| m.fingerprint != fingerprint) {
            return Err(IndexerError::Config(
                "data directory belongs to a different genesis config".into(),
            ));
        }
        let db_memory = DbMemory::new();
        let mut state = StateDb::open_with(&data_dir.join(STATE_DIR), &db_memory)?;
        let genesis = match (state.latest_version(), &meta) {
            (None, _) => {
                // Write the metadata before committing the genesis state: on a crash between the two steps, the next
                // open recommits with the same config
                let writes = genesis_file.state_writes()?;
                let root = state.preview_root(&writes)?;
                let header = genesis_file.header(&params, root)?;
                if meta.as_ref().is_some_and(|m| m.genesis != header) {
                    return Err(IndexerError::Config(
                        "the genesis block in the metadata does not match the genesis config".into(),
                    ));
                }
                Meta {
                    fingerprint,
                    genesis: header.clone(),
                    tip: TipRecord {
                        height: 0,
                        hash: header.hash(),
                        state_root: root,
                    },
                }
                .save(&data_dir)?;
                if state.commit(0, &writes)? != root {
                    return Err(IndexerError::Storage(
                        "the root of the genesis state does not match the precomputed one".into(),
                    ));
                }
                header
            }
            (Some(_), Some(m)) => {
                if genesis_file.header(&params, m.genesis.state_root)? != m.genesis {
                    return Err(IndexerError::Config(
                        "the genesis block in the metadata does not match the genesis config".into(),
                    ));
                }
                if state.oldest_version() == Some(0) && state.root(0)? != m.genesis.state_root {
                    return Err(IndexerError::Config(
                        "the genesis state in the state store does not match the genesis config".into(),
                    ));
                }
                m.genesis.clone()
            }
            (Some(_), None) => {
                return Err(IndexerError::Config(
                    "the data directory's state store is not empty, but the indexer metadata (indexer.json) is missing".into(),
                ))
            }
        };
        let consensus = match pow_algorithm {
            Some(algorithm) => PowConsensus::with_algorithm(&params, &genesis, algorithm),
            None => PowConsensus::new(&params, &genesis),
        }
        .map_err(|e| IndexerError::Config(format!("genesis block: {e}")))?;
        let genesis_hash = genesis.hash();
        let index = Arc::new(Index::open(&data_dir.join(INDEX_DIR), &db_memory)?);
        let latest = state.latest_version().unwrap_or(0);
        let reconciled = latest == 0;

        // Continue from the pruning checkpoint (D-097); the first round verifies it against the state store and the
        // index. A state store at the genesis block, or pruning turned off, starts from the genesis anchor
        let restored = if pruning.enabled && !reconciled {
            restore(&data_dir, &params, &consensus, &genesis_hash, latest)
        } else {
            remove_checkpoint(&data_dir)?;
            None
        };
        let (tracker, known, root, window) = match restored {
            Some(Restored {
                known,
                snapshot,
                root,
                window,
            }) => {
                eprintln!(
                    "[ksana-indexer] continuing from the pruning checkpoint: pruning root at height {}, executed main chain \
                     held from height {}, BTC snapshot from height {} (D-097)",
                    root.1,
                    window.0,
                    snapshot.base()
                );
                (
                    BtcTracker::with_snapshot(btc, params.clone(), &genesis, snapshot)?,
                    known,
                    Some(root),
                    Some(window),
                )
            }
            None => (
                BtcTracker::new(btc, params.clone(), &genesis)?,
                Known::new(params.clone(), consensus.clone(), genesis.clone()).map_err(internal)?,
                None,
                None,
            ),
        };
        let exec = Executor::new(params.clone())
            .map_err(|e| internal(format!("creating the executor: {e}")))?;

        let status = if reconciled {
            index.clear()?;
            index.put(0, &genesis_entry(&genesis), genesis.state_root)?;
            IndexerStatus {
                height: 0,
                hash: genesis_hash,
                state_root: genesis.state_root,
                btc_height: None,
                finalized_height: 0,
                finalized_hash: genesis_hash,
                btc_final_height: 0,
                btc_final_hash: genesis_hash,
                halted: None,
            }
        } else {
            // Before verification against the BTC data: shown per the metadata (the hash is unknown when the metadata
            // lags behind the state store)
            let state_root = state.root(latest)?;
            let hash = meta
                .as_ref()
                .map(|m| &m.tip)
                .filter(|t| t.height == latest && t.state_root == state_root)
                .map_or([0; 32], |t| t.hash);
            IndexerStatus {
                height: latest,
                hash,
                state_root,
                btc_height: None,
                finalized_height: 0,
                finalized_hash: genesis_hash,
                btc_final_height: 0,
                btc_final_hash: genesis_hash,
                halted: None,
            }
        };
        let executed = Arc::new(genesis_main(&genesis, &index));
        Ok(Indexer {
            params,
            data_dir,
            genesis_file,
            genesis,
            fingerprint,
            consensus,
            exec,
            tracker,
            index,
            db_memory,
            pruning,
            known: Mutex::new(known),
            inner: Mutex::new(Inner {
                state: Some(state),
                executed,
                reconciled,
                lagging: false,
                retry: true,
                halted: None,
                validating: None,
                unjudged: None,
                btc_final: None,
                root,
                retain: root.map(|(_, height)| height),
                restored: window,
            }),
            published: RwLock::new(Published {
                status,
                verified: reconciled,
            }),
            round: Mutex::new(()),
            stop: AtomicBool::new(false),
        })
    }

    /// Protocol parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Genesis block header.
    pub fn genesis(&self) -> &BlockHeader {
        &self.genesis
    }

    /// Current status.
    pub fn status(&self) -> IndexerStatus {
        self.published
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .status
            .clone()
    }

    /// The root of the last pruning (hash and height, D-097); `None` before the first one.
    pub fn pruning_root(&self) -> Option<([u8; 32], u64)> {
        self.inner.lock().ok()?.root
    }

    /// The pruning point (D-097): the oldest block of the block DAG held in memory; `None` before the first pruning.
    pub fn pruning_point(&self) -> Option<[u8; 32]> {
        self.known
            .lock()
            .ok()?
            .tree()
            .dag()
            .pruning_point()
            .copied()
    }

    /// The number of blocks in the block DAG held in memory.
    pub fn dag_len(&self) -> usize {
        self.known
            .lock()
            .map(|known| known.tree().dag().len())
            .unwrap_or(0)
    }

    /// The highest verified height, `None` before verification against the BTC data after a restart.
    fn verified_height(&self) -> Option<u64> {
        let published = self
            .published
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        published.verified.then_some(published.status.height)
    }

    /// The block at an executed and verified height: (block hash, state root). `None` when the height has not been
    /// executed (or not yet verified against the BTC data after a restart). Answered from the index, also below the
    /// executed main chain held in memory (D-097).
    pub fn block_at(&self, height: u64) -> Option<([u8; 32], [u8; 32])> {
        if height > self.verified_height()? {
            return None;
        }
        let (entry, state_root) = self.index.get(height).ok()??;
        Some((entry.hash, state_root))
    }

    /// (height, state root) of the executed and verified main-chain block with hash `hash`.
    pub fn find_block(&self, hash: &[u8; 32]) -> Option<(u64, [u8; 32])> {
        let verified = self.verified_height()?;
        let (height, _) = self.index.accepted(hash).ok()??;
        if height > verified {
            return None;
        }
        let (entry, state_root) = self.index.get(height).ok()??;
        (entry.hash == *hash).then_some((height, state_root))
    }

    /// Requests a stop: a [`Indexer::sync_once`] in progress returns after the current block, and later calls do only
    /// the minimum of work.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// One sync round: syncs with bitcoind up to its tip (on a BTC reorg the tracker rescans), accounts for the
    /// pending settlements of known blocks, then follows the processable prefix of the block tree's main chain: rolls
    /// back (when executed blocks leave the main chain or the prefix) and executes block by block, verifying the
    /// `state_root`; then prunes when the pruning root has advanced far enough (D-097).
    ///
    /// The BTC scan goes one tracker round (at most 500 BTC blocks) at a time, and each round is followed by the
    /// finality decision, the processing and the pruning it allows, as the node does after every BTC sync: an indexer
    /// catching up through a long history (a fresh one, or one starting over) keeps only the pruning window as it goes
    /// instead of holding the whole history until it reaches the BTC tip (second audit, P5-2). Only before the state
    /// store is verified against the BTC data after a restart does processing wait for the tip: verifying against part
    /// of the scan would roll back versions that a later round confirms.
    ///
    /// A main-chain block that is invalid or cannot land (invalid body, forced inclusion mismatch, settlement backlog
    /// limit, execution judged invalid, `state_root` mismatch) is marked and the main chain is re-selected (D-063).
    /// Only when that block is BTC-final (D-082) does processing halt, with the reason recorded in
    /// [`IndexerStatus::halted`]; later calls still scan BTC and update the BTC height in the status. Local faults such
    /// as bitcoind or storage return an error, do not judge the block invalid, and are retried in the next round. A BTC
    /// reorg deeper than what pruning kept makes the indexer start over from the genesis anchor (D-097).
    pub fn sync_once(&self) -> Result<Progress, IndexerError> {
        let _round = self.round.lock().map_err(poisoned)?;
        let mut changed = false;
        let mut prunes: u32 = 0;
        loop {
            // One tracker round; `scanned` until it finds nothing new at the BTC tip
            let mut scanned = false;
            if !self.stopping() {
                match self.tracker.sync(self) {
                    Ok(advanced) => scanned = advanced,
                    // The reorg reaches below the BTC blocks kept since pruning (D-097)
                    Err(TrackerError::TooDeep(reason)) if self.pruned()? => {
                        let mut known = self.known.lock().map_err(poisoned)?;
                        let mut inner = self.inner.lock().map_err(poisoned)?;
                        self.reset(&mut known, &mut inner, &reason)?;
                        changed = true;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            let mut round = scanned;
            let snapshot = self.tracker.snapshot()?;
            let btc_height = snapshot.tip().map(|(height, _)| height);
            self.published.write().map_err(poisoned)?.status.btc_height = btc_height;

            let mut known = self.known.lock().map_err(poisoned)?;
            let mut inner = self.inner.lock().map_err(poisoned)?;
            let mut rescan = false;
            // The decision also runs again while it waits for a verdict (D-101): judging needs the final tip executed,
            // which the processing below does
            let mut decide = round || inner.validating.is_some();
            loop {
                if inner.halted.is_none() && decide {
                    match self.finalize(&mut known, &mut inner, &snapshot)? {
                        Round::Go(advanced) => round |= advanced,
                        Round::Rescan => rescan = true,
                    }
                }
                let mut processed = false;
                if !rescan && inner.halted.is_none() && !self.stopping() && (round || inner.retry) {
                    inner.retry = true;
                    // Before the verification after a restart, only once the scan has reached the BTC tip (see above)
                    if inner.reconciled || !scanned {
                        match self.process(&mut known, &mut inner, &snapshot)? {
                            Round::Go(changed) => {
                                processed = changed;
                                round |= changed;
                                if !self.stopping() {
                                    inner.retry = false;
                                    if self.maybe_prune(&mut known, &mut inner, &snapshot)? {
                                        round = true;
                                        prunes = prunes.saturating_add(1);
                                    }
                                }
                            }
                            Round::Rescan => rescan = true,
                        }
                    }
                }
                // The decision waits for a verdict and the processing executed blocks (the final tip among them, D-101):
                // decide again. Each pass needs the processing to change something, so this ends
                decide = processed && inner.validating.is_some();
                if rescan || !decide || inner.halted.is_some() || self.stopping() {
                    break;
                }
            }
            if rescan {
                inner.retry = true;
                round = true;
            }
            changed |= round;
            if rescan || !scanned || self.stopping() {
                return Ok(Progress {
                    height: self.status().height,
                    main_height: known.tree().main_height(),
                    btc_height,
                    finalized_height: known.finalized_height(),
                    changed,
                    prunes,
                });
            }
        }
    }

    /// Whether the block DAG has been pruned (D-097).
    fn pruned(&self) -> Result<bool, IndexerError> {
        Ok(self
            .known
            .lock()
            .map_err(poisoned)?
            .tree()
            .dag()
            .pruning_point()
            .is_some())
    }

    /// Advances the BTC finality decision (D-082) and publishes the final tip; when the final chain contains a block
    /// judged unable to land, processing halts with an alarm. When a BTC reorg revoked the decision the pruning rests
    /// on (deeper than the pruning margin, D-097), everything starts over from the genesis anchor.
    fn finalize(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        snapshot: &Arc<BtcSnapshot>,
    ) -> Result<Round, IndexerError> {
        if known.finality().root_revoked(snapshot) {
            let reason = format!(
                "a BTC reorg revoked the BTC finality of the pruning root at height {} (deeper than the pruning margin)",
                known.finality().root().height
            );
            self.reset(known, inner, &reason)?;
            return Ok(Round::Rescan);
        }
        let tip_before = known.finality().tip();
        let mut excluded = Vec::new();
        let mut rolled_back = false;
        let mut retreats = Vec::new();
        let finalized = loop {
            let stored = |hash: &[u8; 32]| self.index.verdict(hash).map_err(|e| e.to_string());
            let finalized = known.finalize(snapshot, &stored).map_err(internal)?;
            excluded.extend(finalized.outcome.excluded.iter().copied());
            rolled_back |= finalized.outcome.rolled_back;
            retreats.extend(finalized.retreated);
            if finalized.conflict.is_some() {
                break finalized;
            }
            // The decision waits for a verdict on a block that meets the landing condition (D-101): judge it on top
            // of the final tip and decide again; each verdict moves the decision on, so this ends
            match finalized.outcome.validating {
                Some(hash) if self.judge_for_finality(known, inner, snapshot, &hash)? => {}
                _ => break finalized,
            }
        };
        let (tip, tip_height) = (known.finality().tip(), known.finality().tip_height());
        let finality = known.finality();
        let reported = finality.btc_final(snapshot, inner.btc_final.as_ref());
        let before = inner
            .btc_final
            .replace(reported)
            .unwrap_or_else(|| BtcFinal::from(finality.root()));
        let taken_back = finality.taken_back(&before, &reported);
        for (hash, why) in &excluded {
            match why {
                Exclusion::Invalid => eprintln!(
                    "[ksana-indexer] alarm: block {} met the BTC finality condition but is invalid: excluded from the finality \
                     decision together with every block whose selected chain passes through it (D-101)",
                    hex::encode(hash)
                ),
                Exclusion::Unanchored(member) if member == hash => eprintln!(
                    "[ksana-indexer] alarm: block {} is anchored on a BTC block that is not on the BTC best chain: excluded \
                     from the finality decision together with every block whose selected chain passes through it (D-101)",
                    hex::encode(hash)
                ),
                Exclusion::Unanchored(member) => eprintln!(
                    "[ksana-indexer] alarm: block {} merges block {}, which is anchored on a BTC block that is not on the \
                     BTC best chain: excluded from the finality decision together with every block whose selected chain \
                     passes through it (D-101)",
                    hex::encode(hash),
                    hex::encode(member)
                ),
            }
        }
        if finalized.outcome.validating != inner.validating {
            if let Some(hash) = finalized.outcome.validating {
                eprintln!(
                    "[ksana-indexer] the finality decision is waiting for the verdict on block {} (executing it on top of the \
                     final tip, D-101)",
                    hex::encode(hash)
                );
            }
            inner.validating = finalized.outcome.validating;
        }
        if taken_back {
            eprintln!(
                "[ksana-indexer] alarm (highest level): a BTC reorg took back BTC-final block {} at height {}: it reached \
                 the BTC block that decided it, which had BTC_FINALITY_CONFS confirmations; the BTC-final tip is now block \
                 {} at height {} (D-114)",
                hex::encode(before.hash),
                before.height,
                hex::encode(reported.hash),
                reported.height
            );
        } else if rolled_back {
            eprintln!(
                "[ksana-indexer] a BTC reorg revoked finality decisions, taking back nothing reported as BTC-final; the \
                 decided tip is at height {tip_height} (D-082, D-114)"
            );
        }
        for (hash, height) in &retreats {
            eprintln!(
                "[ksana-indexer] alarm: a BTC reorg took the anchor of main-chain block {} at height {height}, or of a block it \
                 merges, off the BTC best chain: the main chain retreats below it and is chosen again (D-104)",
                hex::encode(hash)
            );
        }
        // a rollback is reported above
        if tip != tip_before && !rolled_back {
            eprintln!(
                "[ksana-indexer] decided final: block {} at height {tip_height}, BTC-final once the BTC block that decided \
                 it has BTC_FINALITY_CONFS confirmations (D-082, D-114)",
                hex::encode(tip)
            );
        }
        if (reported.hash, reported.height) != (before.hash, before.height) && !taken_back {
            eprintln!(
                "[ksana-indexer] BTC-final: block {} at height {} (D-114)",
                hex::encode(reported.hash),
                reported.height
            );
        }
        self.publish_final((tip, tip_height), (reported.hash, reported.height))?;
        if let Some(reason) = finalized.conflict {
            let message = format!("the BTC-final chain is invalid: {reason}");
            eprintln!(
                "[ksana-indexer] alarm (highest level): {message}. The fast lane produced an invalid block that has been finalized by BTC, or the implementation has a non-determinism bug; \
                 processing halted (D-082)"
            );
            inner.halted = Some(message.clone());
            self.published.write().map_err(poisoned)?.status.halted = Some(message);
        }
        Ok(Round::Go(
            tip != tip_before || !excluded.is_empty() || !retreats.is_empty(),
        ))
    }

    /// Records the verdict on block `hash` given on `basis` (D-101), in memory and in the index (a cache: when it cannot
    /// be written, the block is judged again after a restart).
    fn record_verdict(
        &self,
        known: &mut Known,
        hash: &[u8; 32],
        valid: bool,
        basis: Option<[u8; 32]>,
    ) {
        known.record_verdict(*hash, valid, basis);
        if let Some(basis) = basis {
            if let Err(e) = self.index.put_verdict(hash, valid, &basis) {
                eprintln!(
                    "[ksana-indexer] cannot keep the verdict on block {} in the index: {e}",
                    hex::encode(hash)
                );
            }
        }
    }

    /// Judges block `hash`, on which the finality decision waits for a verdict (D-101): it meets the landing condition,
    /// its selected parent is the final tip, and no verdict on it holds on the current BTC chain. It is executed on top
    /// of the final tip's state like a main-chain block ([`verdict::judge`]), without committing anything, and the
    /// verdict is recorded (an invalid block is also marked as unable to land). Returns whether a verdict was
    /// recorded; `false` while the final tip is not executed yet (the processing executes it first), and when no
    /// verdict can be given at this BTC tip (judged again once the tip changes).
    fn judge_for_finality(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        snapshot: &Arc<BtcSnapshot>,
        hash: &[u8; 32],
    ) -> Result<bool, IndexerError> {
        let btc_tip = snapshot.tip();
        if inner.unjudged == Some((*hash, btc_tip)) {
            return Ok(false);
        }
        let header = known.tree().header(hash).cloned().ok_or_else(|| {
            internal("the block awaiting a verdict is missing from the block DAG")
        })?;
        let (Some(parent_height), Some(parent)) = (
            header.height.checked_sub(1),
            header.selected_parent().copied(),
        ) else {
            return Err(internal(
                "the block awaiting a verdict is the genesis block",
            ));
        };
        if !inner.reconciled || inner.executed.get(parent_height).map(|e| e.hash) != Some(parent) {
            return Ok(false);
        }
        let unjudged = |inner: &mut Inner, reason: &str| -> Result<bool, IndexerError> {
            eprintln!(
                "[ksana-indexer] no verdict on block {} (height {}) at this BTC tip: {reason}; the finality decision waits, \
                 judging again when the BTC tip changes (D-101)",
                hex::encode(hash),
                header.height
            );
            inner.unjudged = Some((*hash, btc_tip));
            Ok(false)
        };
        let Some(candidate) = known.candidate(hash, snapshot).map_err(internal)? else {
            return unjudged(inner, "a body in its closure cannot be determined yet");
        };
        let basis = verdict::basis(snapshot, &header);
        let state = state_of(&mut inner.state)?;
        let view = match state.view_at(parent_height) {
            Ok(view) => view,
            Err(e) => return unjudged(inner, &format!("the final tip's state: {e}")),
        };
        let mut main = Arc::clone(&inner.executed);
        if main.tip_height() != parent_height {
            Arc::make_mut(&mut main)
                .truncate(parent_height)
                .map_err(internal)?;
        }
        let judge = Judge {
            params: &self.params,
            exec: &self.exec,
            snapshot,
            client: self.tracker.client(),
        };
        let verdict = verdict::judge(&judge, &candidate, main, Arc::new(view), &mut |writes| {
            state
                .preview_root_at(parent_height, writes)
                .map_err(|e| e.to_string())
        });
        match verdict {
            Verdict::Valid(_) => {
                self.record_verdict(known, hash, true, basis);
                Ok(true)
            }
            Verdict::BadBody(reason) | Verdict::Invalid(reason) => {
                eprintln!(
                    "[ksana-indexer] alarm: block {} at height {} meets the BTC finality condition but is invalid ({reason}): \
                     it comes from an attacker or a faulty producer, or the implementation has a non-determinism bug (D-101)",
                    hex::encode(hash),
                    header.height
                );
                self.record_verdict(known, hash, false, basis);
                known.disqualify(hash, basis, snapshot).map_err(internal)?;
                Ok(true)
            }
            Verdict::Unjudged(reason) => unjudged(inner, &reason),
        }
    }

    /// Aligns the executed blocks with the processable prefix and executes new blocks; when a block is invalid or cannot
    /// land, marks it, re-selects the main chain and starts over (halts when the block is BTC-final, D-082).
    /// `Go(changed)` once every processable block is executed, while the state store waits for the processable prefix to
    /// reach its oldest version before it is verified, or when the round was stopped (which leaves `retry` set).
    fn process(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        snapshot: &Arc<BtcSnapshot>,
    ) -> Result<Round, IndexerError> {
        let mut changed = false;
        'plan: loop {
            if inner.state.is_none() {
                // The previous rebuild from genesis failed midway (local fault): try again
                match self.start_over(known, inner, "the previous rebuild did not finish")? {
                    Round::Go(_) => changed = true,
                    Round::Rescan => return Ok(Round::Rescan),
                }
            }
            let plan = known.plan(snapshot, inner.plan_from());
            match self.align(known, inner, &plan)? {
                Round::Go(aligned) => changed |= aligned,
                Round::Rescan => return Ok(Round::Rescan),
            }
            if !inner.reconciled {
                // Waiting for bitcoind to catch up with the state store (see `align`): nothing is executed meanwhile,
                // the next BTC change tries again
                return Ok(Round::Go(changed));
            }
            let start = inner.height().saturating_add(1);
            for height in start..=plan.processable() {
                if self.stopping() {
                    return Ok(Round::Go(changed));
                }
                let hash = plan.entry(height).map(|e| e.hash).unwrap_or_default();
                let reason = match self.execute(known, inner, &plan, snapshot, height) {
                    Ok(()) => {
                        changed = true;
                        continue;
                    }
                    Err(Step::Invalid(reason)) => {
                        eprintln!(
                            "[ksana-indexer] alarm: block {} at height {height} is invalid ({reason}): the nodes reject it, so it comes from an attacker \
                             or a faulty producer, or the implementation has a non-determinism bug; re-selecting the main chain (D-063)",
                            hex::encode(hash)
                        );
                        reason
                    }
                    Err(Step::Disqualify(reason)) => {
                        eprintln!(
                            "[ksana-indexer] alarm: block {} at height {height} cannot land ({reason}), re-selecting the main chain (D-063)",
                            hex::encode(hash)
                        );
                        reason
                    }
                    Err(Step::Fault(e)) => return Err(e),
                };
                let basis = known
                    .tree()
                    .header(&hash)
                    .and_then(|header| verdict::basis(snapshot, header));
                if let Err(e) = known.disqualify(&hash, basis, snapshot) {
                    // A final block is invalid: the main chain cannot be re-selected, only halt and alarm (D-082)
                    self.halt(inner, &plan, height, format!("{reason}; {e}"))?;
                    return Ok(Round::Go(true));
                }
                changed = true;
                continue 'plan;
            }
            return Ok(Round::Go(changed));
        }
    }

    /// Brings the executed blocks in line with the processable prefix (D-048).
    ///
    /// - The first time after startup: the latest version of the state store is verified against the main chain: from
    ///   `min(latest version, processable height)` downwards, finds the highest version whose state root equals the
    ///   `state_root` of the main-chain block and rolls back to it; when none within the rollback range matches,
    ///   rebuilds from genesis. While the processable height is below the oldest version kept (bitcoind is behind, for
    ///   example still syncing or reindexing), no version can be compared yet: waits without verifying, and
    ///   `inner.reconciled` stays false. Continuing from a pruning checkpoint, the executed main chain below the
    ///   pruning root comes from the index (D-097).
    /// - Afterwards: when executed blocks are no longer on the main chain or within the processable prefix, rolls back
    ///   to the highest height still valid. After pruning that is never below the pruning root (D-097): when even the
    ///   root has left the main chain, everything starts over from the genesis anchor.
    fn align(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        plan: &Plan,
    ) -> Result<Round, IndexerError> {
        if inner.reconciled {
            let height = inner.height();
            let mut common = height.min(plan.processable());
            while common >= plan.base
                && inner.executed.get(common).map(|e| e.hash) != plan.entry(common).map(|e| e.hash)
            {
                match common.checked_sub(1) {
                    Some(below) => common = below,
                    None => break,
                }
            }
            if common < plan.base {
                return self.start_over(
                    known,
                    inner,
                    &format!(
                        "the processable main chain no longer passes through the pruning root at height {}",
                        plan.base
                    ),
                );
            }
            if common == height {
                return Ok(Round::Go(false));
            }
            eprintln!(
                "[ksana-indexer] verified up to height {height}, but the blocks from height {} on have left the main chain or the settled prefix (BTC reorg or main-chain switch), rolling back to height {common}",
                common.saturating_add(1)
            );
            return self.rollback(known, inner, common);
        }

        let state = state_of(&mut inner.state)?;
        let latest = state
            .latest_version()
            .ok_or_else(|| IndexerError::Storage("state store is empty".into()))?;
        let oldest = state.oldest_version().unwrap_or(latest);
        if plan.processable() < oldest {
            // The BTC data seen so far does not reach any version kept: that says nothing about whether the state store
            // is valid, so rebuilding from genesis now could throw away a valid store for nothing. Wait until the
            // processable prefix reaches the oldest version kept, then verify as usual (D-048)
            if !inner.lagging {
                eprintln!(
                    "[ksana-indexer] the processable prefix on BTC (height {}) is below the oldest state version kept ({oldest}): waiting for bitcoind \
                     to catch up before verifying the state store (latest version {latest}) against the BTC data (D-048)",
                    plan.processable()
                );
                inner.lagging = true;
            }
            return Ok(Round::Go(false));
        }
        let lowest = oldest.max(plan.base);
        let mut version = latest.min(plan.processable());
        let found = loop {
            if version < lowest {
                break None;
            }
            if Some(state.root(version)?) == plan.state_root(version) {
                break Some(version);
            }
            match version.checked_sub(1) {
                Some(v) => version = v,
                None => break None,
            }
        };
        let Some(version) = found else {
            return self.start_over(
                known,
                inner,
                &format!(
                    "the state store (latest version {latest}) matches the main chain on BTC nowhere within the rollback range"
                ),
            );
        };
        if version < latest {
            eprintln!(
                "[ksana-indexer] state store latest version {latest}, matches the main chain on BTC at height {version}, rolling back to that height"
            );
            state.rollback_to(version)?;
        }
        self.index.rewind(version)?;
        // The executed main chain up to `version`: below the processable prefix (continuing from a pruning checkpoint)
        // from the index, the rest from the prefix, which the index is brought in line with
        let (base, base_prefix) = inner.restored.unwrap_or((plan.base, 0));
        let mut entries = Vec::new();
        for height in base..plan.base {
            match self.index.get(height)? {
                Some((entry, _)) => entries.push(entry),
                None => {
                    return self.start_over(
                        known,
                        inner,
                        &format!("the index lacks the executed block at height {height}"),
                    )
                }
            }
        }
        for height in plan.base..=version {
            let (Some(entry), Some(state_root)) = (plan.entry(height), plan.state_root(height))
            else {
                return Err(internal(
                    "processable prefix shorter than the verified height",
                ));
            };
            if self.index.get(height)?.as_ref().map(|(e, r)| (e, *r)) != Some((entry, state_root)) {
                self.index.put(height, entry, state_root)?;
            }
            entries.push(entry.clone());
        }
        let executed = MainChain::from_window(base, base_prefix, entries)
            .map_err(internal)?
            .with_history(Arc::new(IndexHistory(Arc::clone(&self.index))));
        let (hash, state_root) = match (executed.tip(), plan.state_root(version)) {
            (Some(tip), Some(root)) => (tip.hash, root),
            _ => return Err(internal("empty executed main chain")),
        };
        inner.executed = Arc::new(executed);
        inner.reconciled = true;
        inner.restored = None;
        self.published.write().map_err(poisoned)?.verified = true;
        self.publish_tip(version, hash, state_root)?;
        eprintln!("[ksana-indexer] verification against the BTC data complete: continuing from height {version}");
        Ok(Round::Go(true))
    }

    /// Rolls back to height `to`; beyond the rollback range rebuilds from genesis (after pruning, starts over from the
    /// genesis anchor, D-097).
    fn rollback(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        to: u64,
    ) -> Result<Round, IndexerError> {
        let base = inner.executed.base();
        let state = state_of(&mut inner.state)?;
        if to < base || !state.oldest_version().is_some_and(|oldest| to >= oldest) {
            return self.start_over(
                known,
                inner,
                &format!(
                    "rollback target height {to} is beyond the rollback range (REORG_WINDOW {})",
                    self.params.reorg_window
                ),
            );
        }
        state.rollback_to(to)?;
        let state_root = state.root(to)?;
        self.index.rewind(to)?;
        let executed = Arc::make_mut(&mut inner.executed);
        executed.truncate(to).map_err(internal)?;
        let hash = executed
            .tip()
            .map(|e| e.hash)
            .ok_or_else(|| internal("empty executed main chain"))?;
        self.publish_tip(to, hash, state_root)?;
        Ok(Round::Go(true))
    }

    /// Starts over from the genesis block (`reason` says why): the state is rebuilt from the genesis state; after
    /// pruning, when the pruned data no longer reaches back (D-097), the BTC scan starts over from the genesis anchor
    /// too ([`Round::Rescan`]).
    fn start_over(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        reason: &str,
    ) -> Result<Round, IndexerError> {
        if known.tree().dag().pruning_point().is_some() {
            self.reset(known, inner, reason)?;
            return Ok(Round::Rescan);
        }
        eprintln!("[ksana-indexer] alarm: {reason}, rebuilding from the genesis block");
        self.rebuild(inner)?;
        inner.reconciled = true;
        Ok(Round::Go(true))
    }

    /// Starts over from the genesis anchor (D-097): drops the pruning checkpoint, the BTC snapshot, the block DAG, the
    /// bodies, the finality decision, the index and the state; the next sync scans BTC from the genesis anchor again.
    fn reset(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        reason: &str,
    ) -> Result<(), IndexerError> {
        eprintln!(
            "[ksana-indexer] alarm: {reason}; the data kept since pruning does not reach back that far: starting over from \
             the genesis anchor (D-097)"
        );
        remove_checkpoint(&self.data_dir)?;
        self.tracker.reset()?;
        *known = Known::new(
            self.params.clone(),
            self.consensus.clone(),
            self.genesis.clone(),
        )
        .map_err(internal)?;
        inner.root = None;
        inner.retain = None;
        inner.restored = None;
        inner.validating = None;
        inner.unjudged = None;
        inner.btc_final = None;
        let genesis = (self.genesis.hash(), 0);
        self.publish_final(genesis, genesis)?;
        self.rebuild(inner)?;
        inner.reconciled = true;
        Ok(())
    }

    /// Deletes the state store and the index and starts over from the genesis state.
    fn rebuild(&self, inner: &mut Inner) -> Result<(), IndexerError> {
        // Close the state store first (execution has finished, no read views are still alive)
        inner.state = None;
        let dir = self.data_dir.join(STATE_DIR);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            // The previous rebuild was interrupted after the deletion
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(IndexerError::Storage(format!(
                    "removing {}: {e}",
                    dir.display()
                )))
            }
        }
        let mut state = StateDb::open_with(&dir, &self.db_memory)?;
        let root = state.commit(0, &self.genesis_file.state_writes()?)?;
        if root != self.genesis.state_root {
            return Err(IndexerError::Storage(
                "the rebuilt genesis state does not match the genesis block".into(),
            ));
        }
        self.index.clear()?;
        self.index.put(0, &genesis_entry(&self.genesis), root)?;
        inner.state = Some(state);
        inner.executed = Arc::new(genesis_main(&self.genesis, &self.index));
        self.published.write().map_err(poisoned)?.verified = true;
        self.publish_tip(0, self.genesis.hash(), root)
    }

    /// Executes the main-chain block at `height` (whose selected parent is the executed tip), the same as when a node
    /// connects a main-chain block (D-063), through [`verdict::judge`] (D-101): the body rules of the block and of the
    /// blocks it merges (the nodes reject a block breaking them, with every block after it), the settlement backlog
    /// limit on main-chain blocks (D-046), executing the merged blocks in lenient mode first and then the block itself
    /// in strict mode, verifying the `state_root`; the verdict is recorded on its basis for the finality decision.
    /// Then records the block in the index and commits as state version `height`.
    fn execute(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        plan: &Plan,
        snapshot: &Arc<BtcSnapshot>,
        height: u64,
    ) -> Result<(), Step> {
        let parent_height = height
            .checked_sub(1)
            .ok_or_else(|| internal("cannot execute the genesis block"))?;
        let hash = known
            .tree()
            .main_hash(height)
            .copied()
            .ok_or_else(|| internal(format!("missing main-chain block at height {height}")))?;
        let entry = plan
            .entry(height)
            .cloned()
            .filter(|e| e.hash == hash)
            .ok_or_else(|| {
                internal(format!(
                    "the block at height {height} is not in the processable prefix"
                ))
            })?;
        if inner.height() != parent_height {
            return Err(Step::Fault(internal(
                "the executed height does not match the parent",
            )));
        }
        let candidate = known
            .candidate(&hash, snapshot)
            .map_err(|e| Step::Fault(internal(e)))?
            .ok_or_else(|| internal(format!("missing header or body of height {height}")))?;
        let header = candidate.header.clone();
        if verdict::anchor_invalid(snapshot, &header) {
            eprintln!(
                "[ksana-indexer] the BTC block (height {}) anchored by the block at height {height} is no longer on the best chain, handling in anchor-invalid mode (D-056)",
                header.btc_anchor.height
            );
        }
        let basis = verdict::basis(snapshot, &header);

        let state = state_of(&mut inner.state)?;
        if state.latest_version() != Some(parent_height) {
            return Err(Step::Fault(internal(format!(
                "state store latest version {:?} is not the parent height {parent_height}",
                state.latest_version()
            ))));
        }
        let view = state.view()?;
        let judge = Judge {
            params: &self.params,
            exec: &self.exec,
            snapshot,
            client: self.tracker.client(),
        };
        let verdict = verdict::judge(
            &judge,
            &candidate,
            Arc::clone(&inner.executed),
            Arc::new(view),
            &mut |writes| state.preview_root(writes).map_err(|e| e.to_string()),
        );
        let outcome = match verdict {
            Verdict::Valid(outcome) => outcome,
            Verdict::BadBody(reason) => {
                self.record_verdict(known, &hash, false, basis);
                return Err(Step::Invalid(reason));
            }
            Verdict::Invalid(reason) => {
                self.record_verdict(known, &hash, false, basis);
                return Err(Step::Disqualify(reason));
            }
            Verdict::Unjudged(reason) => {
                // Including a block in anchor-invalid mode whose re-execution failed: a local fault, not judged
                // invalid for now (D-056, D-100)
                eprintln!(
                    "[ksana-indexer] alert: no verdict on the block at height {height}: {reason}"
                );
                return Err(Step::Fault(IndexerError::Execution(format!(
                    "height {height}: {reason}"
                ))));
            }
        };
        self.record_verdict(known, &hash, true, basis);
        let root = header.state_root;
        // The index first: every state version then has its entry (an entry above the latest version is a leftover,
        // rewound at the next start)
        self.index.put(height, &entry, root)?;
        if state.commit(height, &outcome.writes)? != root {
            state.rollback_to(parent_height)?;
            self.index.rewind(parent_height)?;
            return Err(Step::Fault(IndexerError::Storage(
                "the state root after commit does not match the preview".into(),
            )));
        }
        // With pruning on, the states from the pruning root's height on stay, so that a rollback can reach every
        // height the main chain can still switch at (D-097); the states from the final tip's height on stay too, so
        // that a block the finality decision waits on can be judged on top of it (D-101)
        let mut keep_from = height
            .saturating_sub(self.params.reorg_window)
            .min(known.finalized_height());
        if let Some(retain) = inner.retain {
            keep_from = keep_from.min(retain);
        }
        if state
            .oldest_version()
            .is_some_and(|oldest| keep_from > oldest)
        {
            if let Err(e) = state.prune(keep_from) {
                eprintln!("[ksana-indexer] pruning old state failed (will retry later): {e}");
            }
        }
        let entry_hash = entry.hash;
        Arc::make_mut(&mut inner.executed).push(entry);
        self.publish_tip(height, entry_hash, root)?;
        Ok(())
    }

    /// Prunes once the pruning root can advance the pruning point far enough (D-097, see [`ksana_btc::prune`]): the
    /// block DAG keeps the pruning point and its future, the finality decision continues from the root, the executed
    /// main chain keeps a window (the index answers below it), the BTC snapshot drops what nothing kept can ask about,
    /// and a checkpoint is written. The root must be executed: its state's unlock cursor bounds the window. Returns
    /// whether it pruned.
    fn maybe_prune(
        &self,
        known: &mut Known,
        inner: &mut Inner,
        snapshot: &BtcSnapshot,
    ) -> Result<bool, IndexerError> {
        if !self.pruning.enabled || inner.halted.is_some() || !inner.reconciled {
            return Ok(false);
        }
        let Some((btc_tip, _)) = snapshot.tip() else {
            return Ok(false);
        };
        let Some(target) = btc_tip.checked_sub(self.pruning.btc_margin) else {
            return Ok(false);
        };
        let Some((root, root_height)) = known.finality().decided_by(target) else {
            return Ok(false);
        };
        if root_height == 0 || inner.executed.get(root_height).map(|e| e.hash) != Some(root) {
            return Ok(false);
        }
        inner.retain = Some(inner.retain.map_or(root_height, |r| r.max(root_height)));
        let (point, point_score, point_height, point_anchor) = {
            let tree = known.tree();
            let dag = tree.dag();
            let point = match dag.chain_floor(&root, dag.params().min_pruning_depth()) {
                Ok((_, point)) => point,
                // R moved down after a BTC reorg (within the margin): its chain reaches below the current pruning point
                Err(GhostdagError::Pruned(_)) => return Ok(false),
                Err(e) => return Err(internal(e.to_string())),
            };
            let score = |hash: &[u8; 32]| {
                dag.data(hash)
                    .map(|d| d.blue_score)
                    .ok_or_else(|| internal("block missing from the DAG"))
            };
            let point_score = score(&point)?;
            let current = match dag.pruning_point() {
                Some(current) => score(current)?,
                None => 0,
            };
            if point_score < current.saturating_add(self.pruning.step) {
                return Ok(false);
            }
            let header = tree
                .header(&point)
                .ok_or_else(|| internal("block missing from the DAG"))?;
            (point, point_score, header.height, header.btc_anchor.height)
        };
        // Every block in P's future anchors at or above P's anchor, so the forced-inclusion ranges, anchor times and
        // landings it needs are above this BTC height
        let cut = point_anchor.saturating_sub(self.params.native_confirmations);
        // The window keeps everything still asked about: the unlock segment of any state still executable (they all
        // descend from R's state, whose unlock cursor is the lowest), the main-chain blocks still in the DAG, and the
        // blocks whose landing below the cut would otherwise be lost
        let cursor_height = {
            let state = state_of(&mut inner.state)?;
            let Ok(view) = state.view_at(root_height) else {
                // The state at R's height was already discarded (R first appeared below REORG_WINDOW): wait for a
                // later R, whose state is kept
                return Ok(false);
            };
            unlock_cursor(&view)
                .map_err(IndexerError::Storage)?
                .map_or(1, |(height, _)| height.max(1))
        };
        let settled = match settled_height(snapshot, &inner.executed, cut) {
            Ok(settled) => settled,
            Err(e) => {
                eprintln!(
                    "[ksana-indexer] alert: cannot prune at block {}: {e}",
                    hex::encode(point)
                );
                return Ok(false);
            }
        };
        let base = inner.executed.base().max(
            settled
                .saturating_add(1)
                .min(cursor_height)
                .min(point_height),
        );
        let dropped = match known.prune(&point, &root, cut) {
            Ok(dropped) => dropped,
            Err(e) => {
                eprintln!(
                    "[ksana-indexer] alert: cannot prune the block DAG and the finality decision at block {}: {e} (D-097)",
                    hex::encode(point)
                );
                return Ok(false);
            }
        };
        let mut window = (*inner.executed).clone();
        window.prune(base, snapshot).map_err(internal)?;
        let window_hashes: BTreeSet<[u8; 32]> = window
            .entries()
            .iter()
            .flat_map(|e| e.accepted().map(|b| b.hash))
            .collect();
        let main_base_prefix = window.base_prefix();
        inner.executed = Arc::new(window);
        {
            let tree = known.tree();
            let keep = |hash: &[u8; 32]| tree.dag().contains(hash) || window_hashes.contains(hash);
            self.tracker.prune(cut, &keep)?;
        }
        inner.root = Some((root, root_height));
        let kept = known.tree().dag().len();
        let (headers, finality) = known.record().map_err(internal)?;
        let checkpoint = Checkpoint {
            version: CHECKPOINT_VERSION,
            genesis: self.genesis.hash(),
            root: (root, root_height),
            main_base: base,
            main_base_prefix,
            snapshot: self.tracker.snapshot()?.record(),
            headers,
            finality,
        };
        let written = write_checkpoint(&self.data_dir.join(CHECKPOINT_FILE), &checkpoint);
        eprintln!(
            "[ksana-indexer] pruned at block {} (height {point_height}, blue score {point_score}; root at height \
             {root_height}): {dropped} blocks dropped from memory, {kept} kept; executed main chain held from height \
             {base}, BTC snapshot from height {cut} (D-097)",
            hex::encode(point)
        );
        match written {
            Ok(bytes) => eprintln!("[ksana-indexer] pruning checkpoint written ({bytes} bytes)"),
            // The pruning itself stands; a restart then continues from the previous checkpoint, or scans from the
            // genesis anchor
            Err(e) => {
                eprintln!("[ksana-indexer] alert: cannot write the pruning checkpoint: {e} (D-097)")
            }
        }
        match self.write_buffer_bytes(inner.state.as_ref()) {
            Ok(bytes) => eprintln!(
                "[ksana-indexer] RocksDB memory: write buffers {} KiB, block cache {} KiB (D-103)",
                bytes / 1024,
                self.db_memory.block_cache_bytes() / 1024
            ),
            Err(e) => eprintln!("[ksana-indexer] cannot read RocksDB's memory use: {e} (D-103)"),
        }
        Ok(true)
    }

    /// The bytes RocksDB's write buffers hold in the state store and the index (D-103).
    fn write_buffer_bytes(&self, state: Option<&StateDb>) -> Result<u64, IndexerError> {
        let state = match state {
            Some(state) => state.memtable_bytes()?,
            None => 0,
        };
        Ok(state.saturating_add(self.index.memtable_bytes()?))
    }

    /// Halts processing because a BTC-final block is invalid or cannot land, so the main chain cannot be re-selected:
    /// records the reason and raises an alarm (§10.3 item 6, D-082).
    fn halt(
        &self,
        inner: &mut Inner,
        plan: &Plan,
        height: u64,
        reason: String,
    ) -> Result<(), IndexerError> {
        let hash = plan.entry(height).map(|e| e.hash).unwrap_or_default();
        let message = format!(
            "block {} at height {height} is invalid: {reason}",
            hex::encode(hash)
        );
        eprintln!(
            "[ksana-indexer] alarm (highest level): {message}. The fast lane produced an invalid block that has been finalized by BTC, or the implementation has a \
             non-determinism bug; processing halted (D-082)"
        );
        inner.halted = Some(message.clone());
        self.published.write().map_err(poisoned)?.status.halted = Some(message);
        Ok(())
    }

    /// Publishes the executed tip and records it in the metadata.
    fn publish_tip(
        &self,
        height: u64,
        hash: [u8; 32],
        state_root: [u8; 32],
    ) -> Result<(), IndexerError> {
        {
            let mut published = self.published.write().map_err(poisoned)?;
            published.status.height = height;
            published.status.hash = hash;
            published.status.state_root = state_root;
        }
        self.save_tip(TipRecord {
            height,
            hash,
            state_root,
        });
        Ok(())
    }

    /// Publishes the tip of the finality decision (D-082) and the BTC-final tip as reported (D-114).
    fn publish_final(
        &self,
        (hash, height): ([u8; 32], u64),
        (btc_final_hash, btc_final_height): ([u8; 32], u64),
    ) -> Result<(), IndexerError> {
        let mut published = self.published.write().map_err(poisoned)?;
        published.status.finalized_height = height;
        published.status.finalized_hash = hash;
        published.status.btc_final_height = btc_final_height;
        published.status.btc_final_hash = btc_final_hash;
        Ok(())
    }

    /// The metadata is only for the status shown after a restart: a failed write is only logged (re-verified by state
    /// root after a restart).
    fn save_tip(&self, tip: TipRecord) {
        let meta = Meta {
            fingerprint: self.fingerprint,
            genesis: self.genesis.clone(),
            tip,
        };
        if let Err(e) = meta.save(&self.data_dir) {
            eprintln!("[ksana-indexer] writing metadata failed: {e}");
        }
    }
}

impl KnownBlocks for Indexer {
    fn count(&self, snapshot: &BtcSnapshot, header: &BlockHeader) -> Result<bool, TrackerError> {
        // The timestamp bound takes the time of the anchor block on the BTC chain being scanned (D-083, the same as
        // the node's `Chain::anchor_time`; a header is asked about only once its anchor is on that chain, D-135)
        self.known
            .lock()
            .map_err(|_| {
                TrackerError::Internal("the indexer's block tree lock is poisoned".into())
            })?
            .count(header, snapshot)
            .map_err(TrackerError::Internal)
    }
}
