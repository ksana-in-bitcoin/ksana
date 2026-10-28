//! Indexer test utilities: an in-process fast-lane node (the chain and producer of `ksana-node`, without background
//! threads, producing blocks one at a time on demand), an in-memory mock BTC chain (`MockBtc`), native transactions and
//! header republish transactions, opening and syncing the indexer, and block-by-block comparison with the node.

#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{Keypair as BtcKeypair, Secp256k1, SecretKey};
use bitcoin::{
    absolute, transaction, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use ksana_btc::client::BtcClient;
use ksana_btc::data::data_script;
use ksana_btc::mock::MockBtc;
use ksana_btc::native::outpoint_of;
use ksana_consensus::PowAlgorithm;
use ksana_crypto::{account_of, keypair_from_secret, sign_native_vmop, sign_vmop, Keypair};
use ksana_execution::{Allocation, GenesisFile, StateSource};
use ksana_indexer::{Indexer, IndexerConfig, Progress};
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::Chain;
use ksana_node::producer::{Funding, Producer, TemplateSync};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, Block, BlockHeader, Body, BtcAnchor, Entry, FeeMode, Params, Payload, SigScheme, VmOp,
    DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use tokio::sync::broadcast;

pub struct User {
    pub kp: Keypair,
    pub account: [u8; 32],
}

pub fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(format!("ksana/indexer-test/{seed}").as_bytes())).unwrap();
    User {
        account: account_of(&kp),
        kp,
    }
}

/// A path-B KSA transfer op. For path C use [`transfer_native`].
pub fn transfer(u: &User, nonce: u64, fee: FeeMode, to: [u8; 32], amount: u128) -> VmOp {
    let mut op = unsigned_transfer(u, nonce, fee, to, amount);
    op.sig = sign_vmop(&op, &u.kp, &[3; 32]).unwrap();
    op
}

/// A path-C KSA transfer op: the signature binds the carrying transaction's first input `first_input` (D-086); used
/// with [`native_tx`].
pub fn transfer_native(
    u: &User,
    nonce: u64,
    first_input: &bitcoin::OutPoint,
    to: [u8; 32],
    amount: u128,
) -> VmOp {
    let mut op = unsigned_transfer(u, nonce, FeeMode::BtcNative, to, amount);
    op.sig = sign_native_vmop(&op, &outpoint_of(first_input), &u.kp, &[3; 32]).unwrap();
    op
}

fn unsigned_transfer(u: &User, nonce: u64, fee: FeeMode, to: [u8; 32], amount: u128) -> VmOp {
    VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee,
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to,
            amount,
        },
        sig: SigScheme::Bip340([0; 64]),
    }
}

pub fn token(amount: u128) -> FeeMode {
    FeeMode::Token { amount }
}

pub fn btc_secret(seed: &str) -> SecretKey {
    SecretKey::from_slice(&sha256(format!("ksana/indexer-test/btc/{seed}").as_bytes())).unwrap()
}

/// Genesis timestamp of the test chain: 30 days earlier. The weight of an empty block is 12-120 by streak (D-084,
/// D-090), and every block produced advances the ASERT schedule by 12-120 block intervals, so a test chain producing
/// blocks on demand quickly runs far ahead of schedule and the difficulty doubles about every 12 empty blocks; with the
/// genesis timestamp moved earlier the chain always lags the schedule (enough for over 4 000 empty blocks of weight
/// 120), and the difficulty stays at the PoW limit.
pub fn genesis_timestamp() -> u64 {
    ksana_node::unix_now() - 30 * 24 * 3600
}

/// The mock chain (with one block mined first) and a genesis config anchored at its tip.
pub fn setup(allocations: &[(&User, u128)]) -> (Arc<MockBtc>, GenesisFile) {
    let mock = Arc::new(MockBtc::new());
    mock.mine(1);
    let (height, hash) = mock.tip().unwrap();
    let genesis = GenesisFile {
        timestamp: genesis_timestamp(),
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

/// Parameters for the scenario tests (individual tests modify them).
pub const PARAMS: Params = DEVNET;

/// One fast-lane node instance: the chain, the entry pool, a producer with a funding wallet, and a producer without one
/// (producing only empty blocks and blocks containing only forced-inclusion entries). The scenarios here are written as
/// "every block needing a body carries its own settlement transaction", so the producer has settlement batching off
/// (D-077); [`Side::batched`] turns it on.
pub struct Side {
    pub chain: Arc<Chain>,
    pub mempool: Arc<Mutex<Mempool>>,
    pub producer: Producer,
    pub idle: Producer,
    pub funding: Funding,
    pub mock: Arc<MockBtc>,
    _dir: tempfile::TempDir,
}

fn producer(
    chain: &Arc<Chain>,
    mempool: &Arc<Mutex<Mempool>>,
    account: [u8; 32],
    funding: Option<Funding>,
    pack: bool,
) -> Producer {
    Producer {
        chain: chain.clone(),
        mempool: mempool.clone(),
        producer: account,
        funding,
        pack,
        sync: Arc::new(TemplateSync::default()),
        events: broadcast::channel(16).0,
        shutdown: Arc::new(AtomicBool::new(false)),
        paused: Arc::new(AtomicBool::new(false)),
        min_interval: Duration::ZERO,
        announce: None,
        cache: Default::default(),
    }
}

impl Side {
    /// Runs with [`PARAMS`].
    pub fn new(mock: &Arc<MockBtc>, genesis: &GenesisFile, name: &str) -> Side {
        Side::with_algorithm(mock, genesis, name, None)
    }

    /// Same as [`Side::new`], with settlement batching on for the producer (D-077).
    pub fn batched(mock: &Arc<MockBtc>, genesis: &GenesisFile, name: &str) -> Side {
        Side::open(mock, genesis, name, None, PARAMS, true)
    }

    /// With the given mining algorithm (D-062; `None` is the one selected by the network params).
    pub fn with_algorithm(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        algorithm: Option<Arc<dyn PowAlgorithm>>,
    ) -> Side {
        Side::open(mock, genesis, name, algorithm, PARAMS, false)
    }

    /// Runs with the given params (e.g. DEVNET, matching the indexer binary with `--chain devnet`).
    pub fn with_params(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        params: Params,
    ) -> Side {
        Side::open(mock, genesis, name, None, params, false)
    }

    fn open(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        algorithm: Option<Arc<dyn PowAlgorithm>>,
        params: Params,
        batching: bool,
    ) -> Side {
        let dir = tempfile::tempdir().unwrap();
        let chain = Arc::new(
            Chain::open_with_algorithm(
                dir.path(),
                params.clone(),
                genesis,
                mock.clone(),
                algorithm,
            )
            .unwrap(),
        );
        let secret = btc_secret(&format!("{name}/funding"));
        let funding = Funding::new(BtcKeypair::from_secret_key(&Secp256k1::new(), &secret), 2)
            .with_batching(batching);
        mock.fund(&funding.script, &[400_000; 6]);
        while chain.sync_btc().unwrap() {}
        let mempool = Arc::new(Mutex::new(Mempool::new(params, Policy::default())));
        let account = user(&format!("{name}/producer")).account;
        Side {
            producer: producer(&chain, &mempool, account, Some(funding.clone()), true),
            // Produces only empty blocks (or blocks containing only forced-inclusion entries), without settlement
            // transactions; a producer without a funding wallet would normally pack too (D-095), turned off here
            idle: producer(&chain, &mempool, account, None, false),
            chain,
            mempool,
            funding,
            mock: mock.clone(),
            _dir: dir,
        }
    }

    /// Syncs with the mock chain (a real node syncs periodically from a background thread) until nothing changes and,
    /// like the node, maintains the entry pool after the main-chain changes this caused (D-041, D-082, D-104).
    pub fn sync(&self) {
        while self.chain.sync_btc().unwrap() {}
        ksana_node::ops::apply_chain_changes(&self.chain, &self.mempool, &self.producer.events)
            .unwrap();
    }

    /// Accepts another node's block (handled as syncing a historical block).
    pub fn accept(&self, block: &Block) {
        self.sync();
        self.chain.accept_block(block.clone(), None).unwrap();
    }

    fn step(&self, producer: &Producer) -> Block {
        self.sync();
        for _ in 0..20 {
            if let Some(block) = producer.step().unwrap() {
                return block;
            }
        }
        panic!("block production failed");
    }

    /// Produces one block (including the ops in the entry pool and the unlockable token fees, with a settlement
    /// transaction when needed).
    pub fn mine(&self) -> Block {
        self.step(&self.producer)
    }

    /// Produces one empty block (or a block containing only forced-inclusion entries): no settlement transaction.
    pub fn mine_empty(&self) -> Block {
        let block = self.step(&self.idle);
        assert!(block.settlement.is_none());
        assert!(block
            .entries
            .iter()
            .all(|e| matches!(e, Entry::Forced { .. })));
        block
    }

    /// Puts a path-B op into the entry pool.
    pub fn submit(&self, op: VmOp) {
        let (_, view) = self.chain.tip_and_view().unwrap();
        let state: Arc<dyn StateSource> = Arc::new(view);
        self.mempool.lock().unwrap().insert(op, &state).unwrap();
    }

    /// Broadcasts the settlement transactions of blocks with `SETTLEMENT_DEPTH` confirmations (a real node does this
    /// from a background thread).
    pub fn broadcast_settlements(&self) {
        self.sync();
        self.chain.process_settlements().unwrap();
    }

    pub fn tip_height(&self) -> u64 {
        self.chain.tip().unwrap().header.height
    }

    pub fn block(&self, height: u64) -> Block {
        let hash = self.chain.store().hash_at(height).unwrap().unwrap();
        self.chain.store().block(&hash).unwrap().unwrap()
    }

    /// The highest height of the node's settled prefix (D-038).
    pub fn settled_height(&self) -> u64 {
        self.sync();
        let snapshot = self.chain.btc().snapshot().unwrap();
        (0..=self.tip_height())
            .rev()
            .find(|h| self.chain.prefix_height(&snapshot, *h).unwrap().is_some())
            .unwrap()
    }
}

/// A transaction with a single KSN1 data output (the input is fresh funds from the mock chain).
fn data_tx(mock: &MockBtc, payload: &[u8]) -> Transaction {
    let input = mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0];
    data_tx_from(input, payload)
}

/// A transaction spending `input` with a single KSN1 data output.
fn data_tx_from(input: bitcoin::OutPoint, payload: &[u8]) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: input,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: data_script(&DEVNET.magic, payload).unwrap(),
        }],
    }
}

/// A path-C native transaction (§7.4): spends fresh funds from the mock chain and carries the op given by
/// `op_for(first input)` (whose signature binds it, D-086). Returns the transaction and the op.
pub fn native_tx(
    mock: &MockBtc,
    op_for: impl FnOnce(&bitcoin::OutPoint) -> VmOp,
) -> (Transaction, VmOp) {
    let input = mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0];
    let op = op_for(&input);
    let payload = Payload::NativeOp { op: op.clone() }
        .encode(&PARAMS)
        .unwrap();
    (data_tx_from(input, &payload), op)
}

/// A transaction with a KSN1 envelope in the witness (D-081): spends fresh funds from the mock chain; the witness is a
/// placeholder signature, the envelope leaf and a placeholder control block (the mock chain does not verify scripts or
/// signatures, and the tracker only looks at the witness syntax).
fn envelope_tx(mock: &MockBtc, payload: &[u8]) -> Transaction {
    let input = mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0];
    let key =
        bitcoin::key::Keypair::from_seckey_slice(&bitcoin::key::Secp256k1::new(), &[0x5a; 32])
            .unwrap()
            .x_only_public_key()
            .0;
    let leaf = ksana_btc::envelope::envelope_script(&key, &DEVNET.magic, payload).unwrap();
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: input,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[vec![0u8; 64], leaf.into_bytes(), vec![0xc0; 33]]),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(ksana_btc::settlement::P2A_AMOUNT_SAT),
            script_pubkey: ksana_btc::settlement::p2a_script(),
        }],
    }
}

/// A header republish transaction (§7.1; the payload is in the witness envelope, D-081).
pub fn headers_tx(mock: &MockBtc, headers: Vec<BlockHeader>) -> Transaction {
    republish_tx(mock, headers, Vec::new())
}

/// A republish transaction carrying headers and the bodies of some of them (D-077): each body is the index of its
/// header in `headers` and that block's entries.
pub fn republish_tx(
    mock: &MockBtc,
    headers: Vec<BlockHeader>,
    bodies: Vec<(u32, Vec<Entry>)>,
) -> Transaction {
    let bodies = bodies
        .into_iter()
        .map(|(header, entries)| Body {
            header,
            entries: ksana_btc::payload::entry_data(&entries),
        })
        .collect();
    envelope_tx(
        mock,
        &Payload::Headers {
            headers,
            bodies,
            payee: [0; 32],
        }
        .encode(&PARAMS)
        .unwrap(),
    )
}

/// The previous outputs of the transaction's inputs (read from the mock chain's transaction index).
pub fn prevouts(mock: &MockBtc, tx: &Transaction) -> Vec<TxOut> {
    tx.input
        .iter()
        .map(|input| {
            let info = mock
                .transaction(&input.previous_output.txid.to_byte_array())
                .unwrap()
                .unwrap();
            info.tx.output[input.previous_output.vout as usize].clone()
        })
        .collect()
}

/// Opens an indexer with [`PARAMS`].
pub fn open_indexer(dir: &Path, genesis: &GenesisFile, mock: &Arc<MockBtc>) -> Indexer {
    open_indexer_with(dir, genesis, mock, PARAMS)
}

/// Opens an indexer with the given mining algorithm (D-062).
pub fn open_indexer_with_algorithm(
    dir: &Path,
    genesis: &GenesisFile,
    mock: &Arc<MockBtc>,
    algorithm: Arc<dyn PowAlgorithm>,
) -> Indexer {
    Indexer::open(IndexerConfig {
        data_dir: dir.to_path_buf(),
        genesis: genesis.clone(),
        params: PARAMS,
        btc: mock.clone(),
        pow_algorithm: Some(algorithm),
        pruning: ksana_btc::prune::Pruning::default(),
    })
    .unwrap()
}

/// Opens an indexer with the given params (e.g. a smaller `REORG_WINDOW`, which does not affect execution results).
pub fn open_indexer_with(
    dir: &Path,
    genesis: &GenesisFile,
    mock: &Arc<MockBtc>,
    params: Params,
) -> Indexer {
    Indexer::open(IndexerConfig {
        data_dir: dir.to_path_buf(),
        genesis: genesis.clone(),
        params,
        btc: mock.clone(),
        pow_algorithm: None,
        pruning: ksana_btc::prune::Pruning::default(),
    })
    .unwrap()
}

/// Syncs until nothing changes.
pub fn catch_up(ix: &Indexer) -> Progress {
    for _ in 0..10 {
        let progress = ix.sync_once().unwrap();
        if !progress.changed {
            return progress;
        }
    }
    panic!("indexer sync did not converge");
}

/// The indexer has executed up to `SETTLEMENT_DEPTH` blocks before the node's settled prefix (a main-chain block is
/// only executed once it has that many confirmations on the indexer's main chain, D-063; with a single node the tip of
/// the indexer's main chain is the tip of the settled prefix) or up to the BTC-final block (D-082, no confirmations
/// needed any more), and the block hash and `state_root` at every height are the same as the node's; the final tip, and
/// the BTC-final tip reported (D-114), are the same as the node's.
pub fn assert_matches_node(ix: &Indexer, side: &Side) {
    // The node advances the finality decision on BTC sync: let it see the current BTC chain first
    side.sync();
    let status = ix.status();
    assert_eq!(status.halted, None);
    let (final_hash, final_height) = side.chain.finalized().unwrap();
    assert_eq!(
        (status.finalized_height, status.finalized_hash),
        (final_height, final_hash),
        "the indexer's and the node's final tips should be the same"
    );
    let (btc_final_hash, btc_final_height) = side.chain.btc_final().unwrap();
    assert_eq!(
        (status.btc_final_height, status.btc_final_hash),
        (btc_final_height, btc_final_hash),
        "the indexer and the node should report the same BTC-final tip"
    );
    assert_eq!(
        status.height,
        side.settled_height()
            .saturating_sub(side.chain.params().settlement_depth)
            .max(final_height),
        "the indexer's height should equal the node's settled prefix minus SETTLEMENT_DEPTH (not below the final height)"
    );
    for height in 0..=status.height {
        let block = side.block(height);
        assert_eq!(
            ix.block_at(height),
            Some((block.header.hash(), block.header.state_root)),
            "height {height}"
        );
    }
    assert_eq!(ix.block_at(status.height + 1), None);
    let tip = side.block(status.height);
    assert_eq!(status.hash, tip.header.hash());
    assert_eq!(status.state_root, tip.header.state_root);
    assert_eq!(status.btc_height, Some(side.mock.tip_height()));
}
