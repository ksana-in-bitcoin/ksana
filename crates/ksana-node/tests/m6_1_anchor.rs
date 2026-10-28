//! M6.1: anchor depth and historical blocks with an invalid anchor (D-056). Without the network: node instances share
//! the same mock BTC chain and use the `Chain` interface to produce and accept blocks (`now` given means accepting a
//! new block, `None` means syncing a historical block).

mod common;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::Transaction;
use common::{transfer_op, user, User};
use ksana_btc::client::{BtcClient, BtcError, HeaderInfo, ScannedUtxo, TxInfo, UtxoInfo};
use ksana_btc::mock::MockBtc;
use ksana_consensus::Consensus;
use ksana_execution::keys;
use ksana_execution::StateSource;
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::{Accepted, Chain};
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::producer::{Funding, Producer, TemplateSync};
use ksana_node::{unix_now, NodeError};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Block, BtcAnchor, Params, DEVNET, KSA_ASSET_ID};
use tokio::sync::broadcast;

/// One node instance: chain, entry pool and a producer (whose loop is not running).
struct Side {
    chain: Arc<Chain>,
    mempool: Arc<Mutex<Mempool>>,
    producer: Producer,
    _dir: tempfile::TempDir,
}

impl Side {
    fn new(mock: &Arc<MockBtc>, genesis: &GenesisFile, name: &str, params: &Params) -> Side {
        Side::with_client(mock, mock.clone(), genesis, name, params)
    }

    /// Reaches the mock chain through `client`.
    fn with_client(
        mock: &Arc<MockBtc>,
        client: Arc<dyn BtcClient>,
        genesis: &GenesisFile,
        name: &str,
        params: &Params,
    ) -> Side {
        let dir = tempfile::tempdir().unwrap();
        let chain = Arc::new(Chain::open(dir.path(), params.clone(), genesis, client).unwrap());
        let secret =
            SecretKey::from_slice(&sha256(format!("m6.1/{name}/funding").as_bytes())).unwrap();
        // Every block that needs a body carries its own settlement transaction (settlement batching off, D-077)
        let funding = Funding::new(Keypair::from_secret_key(&Secp256k1::new(), &secret), 2)
            .with_batching(false);
        mock.fund(&funding.script, &[400_000; 4]);
        while chain.sync_btc().unwrap() {}
        let mempool = Arc::new(Mutex::new(Mempool::new(params.clone(), Policy::default())));
        let producer = Producer {
            chain: chain.clone(),
            mempool: mempool.clone(),
            producer: user(&format!("m6.1/{name}/producer")).account,
            funding: Some(funding),
            pack: true,
            sync: Arc::new(TemplateSync::default()),
            events: broadcast::channel(16).0,
            shutdown: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            min_interval: Duration::ZERO,
            announce: None,
            cache: Default::default(),
        };
        Side {
            chain,
            mempool,
            producer,
            _dir: dir,
        }
    }

    /// Syncs with the mock chain (a real node syncs periodically from a background thread) until nothing changes and,
    /// like the node, maintains the entry pool after the main-chain changes this caused (D-041, D-104).
    fn sync(&self) {
        while self.chain.sync_btc().unwrap() {}
        ksana_node::ops::apply_chain_changes(&self.chain, &self.mempool, &self.producer.events)
            .unwrap();
    }

    /// Produces one block (retrying until it succeeds if necessary).
    fn mine(&self) -> Block {
        self.sync();
        for _ in 0..20 {
            if let Some(block) = self.producer.step().unwrap() {
                return block;
            }
        }
        panic!("block production failed");
    }

    fn submit(&self, op: ksana_primitives::VmOp) {
        let (_, view) = self.chain.tip_and_view().unwrap();
        let state: Arc<dyn StateSource> = Arc::new(view);
        self.mempool.lock().unwrap().insert(op, &state).unwrap();
    }

    /// Accepts as a newly received block (syncing BTC first).
    fn accept_new(&self, block: &Block) -> Result<Accepted, NodeError> {
        self.sync();
        self.chain
            .accept_block(block.clone(), Some(unix_now()))
            .map(|r| r.outcome)
    }

    /// Accepts as a historical block (sync response, import).
    fn accept_history(&self, block: &Block) -> Result<Accepted, NodeError> {
        self.sync();
        self.chain
            .accept_block(block.clone(), None)
            .map(|r| r.outcome)
    }

    fn tip_height(&self) -> u64 {
        self.chain.tip().unwrap().header.height
    }

    fn block(&self, height: u64) -> Block {
        let hash = self.chain.store().hash_at(height).unwrap().unwrap();
        self.chain.store().block(&hash).unwrap().unwrap()
    }

    fn ksa(&self, u: &User) -> u128 {
        self.chain
            .with_state(|state, _| {
                state
                    .get(&keys::balance(&KSA_ASSET_ID, &u.account))
                    .unwrap()
                    .map(|v| borsh::from_slice::<u128>(&v).unwrap())
                    .unwrap_or(0)
            })
            .unwrap()
    }
}

fn setup(allocations: &[(&User, u128)]) -> (Arc<MockBtc>, GenesisFile) {
    let mock = Arc::new(MockBtc::new());
    mock.mine(1);
    let (height, hash) = mock.tip().unwrap();
    let genesis = GenesisFile {
        timestamp: unix_now(),
        btc_anchor: BtcAnchor { height, hash },
        bits: None,
        block_share_account: None,
        allocations: allocations
            .iter()
            .map(|(u, amount)| Allocation {
                account: u.account,
                amount: *amount,
            })
            .collect(),
    };
    (mock, genesis)
}

fn is_extended(outcome: Accepted) -> bool {
    matches!(outcome, Accepted::Extended(_))
}

/// A BTC client on the mock chain that runs a hook once, the next time transactions are checked against bitcoind's
/// mempool (`testmempoolaccept`): in the producer, that is after solving a block and before committing it.
struct Racing {
    mock: Arc<MockBtc>,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl BtcClient for Racing {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        self.mock.tip()
    }

    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        self.mock.block_hash(height)
    }

    fn block(&self, hash: &[u8; 32]) -> Result<Option<bitcoin::Block>, BtcError> {
        self.mock.block(hash)
    }

    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        self.mock.header(hash)
    }

    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        self.mock.transaction(txid)
    }

    fn utxo(
        &self,
        outpoint: &bitcoin::OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        self.mock.utxo(outpoint, include_mempool)
    }

    fn scan_utxos(&self, script: &bitcoin::Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        self.mock.scan_utxos(script)
    }

    fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.mock.test_accept(txs)
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        self.mock.broadcast(tx)
    }

    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        self.mock.in_mempool(txid)
    }

    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        self.mock.mempool_txids()
    }

    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        self.mock.txindex_height()
    }
}

/// `ANCHOR_DEPTH` > 0: the producer anchors at `max(parent anchor height, tip − ANCHOR_DEPTH)`; when accepting a new
/// block, an anchor above that height is treated as the local BTC lagging behind (retry later, not judged invalid),
/// and an anchor more than `MAX_ANCHOR_LAG + ANCHOR_DEPTH` behind the tip is rejected; syncing historical blocks is
/// subject to the first limit too (D-101), not to the second.
#[test]
fn anchor_depth_keeps_new_blocks_below_the_btc_tip() {
    let params = Params {
        anchor_depth: 2,
        ..DEVNET
    };
    let (alice, bob) = (user("m6.1/alice"), user("m6.1/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "a", &params);
    let b = Side::new(&mock, &genesis, "b", &params);
    mock.mine(3);

    // The producer anchors at tip − ANCHOR_DEPTH, and the receiver accepts by the new-block rules
    a.submit(transfer_op(&alice, 0, 2, bob.account, 5));
    let first = a.mine();
    let tip = mock.tip_height();
    assert_eq!(first.header.btc_anchor.height, tip - 2);
    assert_eq!(
        Some(first.header.btc_anchor.hash),
        mock.block_hash(tip - 2).unwrap()
    );
    assert!(is_extended(b.accept_new(&first).unwrap()));
    mock.mine(1);
    let second = a.mine();
    assert_eq!(second.header.btc_anchor.height, tip - 1);
    assert!(is_extended(b.accept_new(&second).unwrap()));
    assert_eq!(b.ksa(&bob), 5);

    // A block anchored at the tip (the producer did not anchor by ANCHOR_DEPTH): treated as the local BTC lagging
    // behind, not judged invalid
    let c = Side::new(&mock, &genesis, "c", &DEVNET);
    for height in 1..=a.tip_height() {
        assert!(is_extended(c.accept_history(&a.block(height)).unwrap()));
    }
    let eager = c.mine();
    assert_eq!(eager.header.btc_anchor.height, mock.tip_height());
    match b.accept_new(&eager) {
        Err(NodeError::Btc(m)) => assert!(m.contains("ANCHOR_DEPTH"), "{m}"),
        other => panic!("should be treated as the local BTC lagging behind: {other:?}"),
    }
    // The same as a historical block (D-101): a BTC reorg of ANCHOR_DEPTH blocks could orphan its anchor
    match b.accept_history(&eager) {
        Err(NodeError::Btc(m)) => assert!(m.contains("ANCHOR_DEPTH"), "{m}"),
        other => panic!("should be treated as the local BTC lagging behind: {other:?}"),
    }
    assert!(!b.chain.is_invalid(&eager.header.hash()).unwrap());
    // After BTC advances another ANCHOR_DEPTH blocks, the same block is accepted by the new-block rules (lag 2 ≤
    // MAX_ANCHOR_LAG + ANCHOR_DEPTH = 3)
    mock.mine(2);
    assert!(is_extended(b.accept_new(&eager).unwrap()));

    // A new block more than MAX_ANCHOR_LAG + ANCHOR_DEPTH behind the tip is rejected; accepted as usual when syncing
    // historical blocks
    assert!(is_extended(a.accept_history(&eager).unwrap()));
    let late = a.mine();
    assert_eq!(late.header.btc_anchor, eager.header.btc_anchor);
    mock.mine(2);
    match b.accept_new(&late) {
        Err(NodeError::Rejected(m)) => assert!(m.contains("MAX_ANCHOR_LAG"), "{m}"),
        other => panic!("should be rejected: {other:?}"),
    }
    assert!(is_extended(b.accept_history(&late).unwrap()));
    assert_eq!(b.chain.tip().unwrap().hash, a.chain.tip().unwrap().hash);
}

/// After the anchor block is reorged out of BTC, the blocks anchored on it leave the main chain (D-104): the node
/// retreats below the lowest of them, and their ops go back into the entry pool and are included again by a block
/// anchored on the new branch. As new blocks the old ones are rejected (depends on the local view, not judged invalid),
/// and as historical blocks they are taken in anchor-invalid mode, but only as side blocks (D-101), so a new node
/// syncing from genesis takes the same main chain. The BTC block holding the funding inputs and the producer's funding
/// is reorged out too and then confirmed again on the new branch (same transaction ID).
#[test]
fn blocks_anchored_to_a_reorged_out_btc_block_leave_the_main_chain() {
    let (carol, dave) = (user("m6.1/carol"), user("m6.1/dave"));
    let (mock, genesis) = setup(&[(&carol, 1_000)]);
    // The new node b has only the genesis block; A is funded afterwards, A's funding transaction is in block X at the
    // current BTC tip, and the following blocks all anchor at X
    let b = Side::new(&mock, &genesis, "b2", &DEVNET);
    let a = Side::new(&mock, &genesis, "a2", &DEVNET);
    a.submit(transfer_op(&carol, 0, 2, dave.account, 5));
    let first = a.mine();
    a.submit(transfer_op(&carol, 1, 2, dave.account, 6));
    let second = a.mine();
    let x = first.header.btc_anchor.clone();
    assert_eq!(second.header.btc_anchor, x);
    assert!(first.settlement.is_some() && second.settlement.is_some());
    assert_eq!(a.ksa(&dave), 11);

    // BTC reorgs out X (its transactions return to the mempool) and the new branch is longer: the main chain retreats
    // to the genesis block, and the two ops go back into the entry pool
    mock.reorg(1);
    mock.mine(2);
    assert!(!mock.header(&x.hash).unwrap().unwrap().in_best_chain);
    a.sync();
    assert_eq!(a.tip_height(), 0);
    assert_eq!(a.ksa(&dave), 0);
    assert_eq!(a.mempool.lock().unwrap().len(), 2);
    // The next block anchors on the new branch and includes them again with a third one
    a.submit(transfer_op(&carol, 2, 2, dave.account, 7));
    let third = a.mine();
    assert_eq!(third.header.height, 1);
    assert!(third.header.btc_anchor.height > x.height);
    assert_eq!(third.entries.len(), 3);
    assert_eq!(a.ksa(&dave), 18);

    // The new node syncs from genesis. The blocks anchored at X are at most MAX_ANCHOR_LAG behind the BTC tip and enter
    // anchor-invalid mode via "a stale block known to bitcoind", as side blocks only; the new block extends
    assert!(mock.tip_height() - x.height <= DEVNET.max_anchor_lag + DEVNET.anchor_depth);
    for block in [&first, &second] {
        // As a new block: the anchor is not on the local BTC best chain, rejected but not marked invalid
        match b.accept_new(block) {
            Err(NodeError::Rejected(m)) => assert!(m.contains("best chain"), "{m}"),
            other => panic!("should be rejected: {other:?}"),
        }
        assert!(!b.chain.is_invalid(&block.header.hash()).unwrap());
        assert_eq!(b.accept_history(block).unwrap(), Accepted::Side);
    }
    assert!(is_extended(b.accept_history(&third).unwrap()));
    assert_eq!(b.chain.tip().unwrap().hash, third.header.hash());
    assert_eq!(b.ksa(&dave), 18);
}

/// A BTC reorg synced while the producer finishes a block, after solving it and before committing it (here while its
/// settlement transactions are checked against bitcoind), can take the block's anchor off the BTC best chain while the
/// main chain below stays eligible: the block is not committed, since it could never become BTC final (D-104), and the
/// next one anchors on the new branch with the same op.
#[test]
fn a_block_whose_anchor_a_reorg_takes_while_it_is_produced_is_not_committed() {
    let (frank, gina) = (user("m6.1/frank"), user("m6.1/gina"));
    let (mock, genesis) = setup(&[(&frank, 1_000)]);
    let racing = Arc::new(Racing {
        mock: mock.clone(),
        hook: Mutex::new(None),
    });
    let a = Side::with_client(&mock, racing.clone(), &genesis, "a5", &DEVNET);
    let first = a.mine();

    // The next block anchors on X, one BTC block above the main-chain tip's anchor; while it is checked, X is reorged
    // out and the node syncs the longer branch
    mock.mine(1);
    let (_, x) = mock.tip().unwrap();
    a.submit(transfer_op(&frank, 0, 2, gina.account, 5));
    a.sync();
    let chain = a.chain.clone();
    let reorg = mock.clone();
    *racing.hook.lock().unwrap() = Some(Box::new(move || {
        reorg.reorg(1);
        reorg.mine(2);
        while chain.sync_btc().unwrap() {}
    }));
    assert!(a.producer.step().unwrap().is_none());
    assert!(racing.hook.lock().unwrap().is_none());
    assert!(!mock.header(&x).unwrap().unwrap().in_best_chain);
    assert_eq!(a.chain.tip().unwrap().hash, first.header.hash());
    assert_eq!(a.ksa(&gina), 0);

    // The next block anchors on the new branch and includes the op
    let next = a.mine();
    assert_eq!(next.header.selected_parent(), Some(&first.header.hash()));
    assert_eq!(
        mock.block_hash(next.header.btc_anchor.height).unwrap(),
        Some(next.header.btc_anchor.hash)
    );
    assert_eq!(next.entries.len(), 1);
    assert_eq!(a.ksa(&gina), 5);
}

/// A forged anchor hash cannot bypass the new-block rules via a sync response (D-056): when a freshly produced block's
/// anchor is not on the local BTC best chain, is not a stale block known to bitcoind, and is not more than
/// `MAX_ANCHOR_LAG + ANCHOR_DEPTH` behind the tip, it is rejected as a historical block as well (not judged invalid,
/// retry later). Once the anchor lags further behind, it is taken in anchor-invalid mode, but only as a side block: a
/// block anchored off the BTC best chain could never become BTC-final, nor could any block on top of it or merging it
/// (D-101), so it never becomes the main-chain tip and no template merges it.
#[test]
fn a_fresh_block_with_an_unknown_off_chain_anchor_is_not_synced_as_history() {
    let erin = user("m6.1/erin");
    let (mock, genesis) = setup(&[(&erin, 1_000)]);
    let a = Side::new(&mock, &genesis, "a3", &DEVNET);
    let b = Side::new(&mock, &genesis, "b3", &DEVNET);
    // An empty block (no settlement transaction, so no settlement payload commits to the original header) gets an
    // anchor hash that never existed (same height), re-solving the PoW
    let block = a.mine();
    assert!(block.entries.is_empty() && block.settlement.is_none());
    let mut forged = block.clone();
    forged.header.btc_anchor.hash = [0x42; 32];
    forged.header.nonce = 0;
    let weight = a.chain.weight_of(&block.header.hash()).unwrap().unwrap();
    assert!(b
        .chain
        .consensus()
        .solve(&mut forged.header, weight, 0, 1 << 24)
        .unwrap());
    for (what, outcome) in [
        ("new block", b.accept_new(&forged)),
        ("historical block", b.accept_history(&forged)),
    ] {
        match outcome {
            Err(NodeError::Rejected(m)) => assert!(m.contains("best chain"), "{what}: {m}"),
            other => panic!("{what} should be rejected: {other:?}"),
        }
    }
    assert!(!b.chain.is_invalid(&forged.header.hash()).unwrap());
    assert_eq!(b.tip_height(), 0);

    // BTC advances until the anchor lags more than MAX_ANCHOR_LAG + ANCHOR_DEPTH: taken in anchor-invalid mode as a
    // historical block, as a side block only (D-101)
    mock.mine(DEVNET.max_anchor_lag + DEVNET.anchor_depth + 1);
    assert_eq!(b.accept_history(&forged).unwrap(), Accepted::Side);
    assert_eq!(b.tip_height(), 0);
    // The original block, anchored on the BTC best chain, still extends as a historical block
    assert!(is_extended(b.accept_history(&block).unwrap()));
}
