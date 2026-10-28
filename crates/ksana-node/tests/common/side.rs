//! Node instances without a network: chain, entry pool and a producer (whose loop is not run); several instances share
//! one mock BTC chain, and the test produces blocks by hand and passes them between the instances. Shared by the M4
//! fork tests and the M7 DAG tests.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use ksana_btc::client::BtcClient;
use ksana_btc::mock::MockBtc;
use ksana_execution::keys::{self, AccountState};
use ksana_execution::StateSource;
use ksana_mempool::{Mempool, Policy};
use ksana_node::chain::{AcceptReport, Chain, ChainOptions, Pruning};
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::producer::{Funding, Producer, TemplateSync};
use ksana_node::NodeError;
use ksana_primitives::hash::sha256;
use ksana_primitives::{Block, BtcAnchor, Params, VmOp, DEVNET, KSA_ASSET_ID};
use tokio::sync::broadcast;

use super::{genesis_timestamp, user, User};

/// A node instance.
pub struct Side {
    pub chain: Arc<Chain>,
    pub mempool: Arc<Mutex<Mempool>>,
    pub producer: Producer,
    pub dir: tempfile::TempDir,
}

impl Side {
    /// Open with the DEVNET params; the producer's funding address has 4 confirmed UTXOs. The scenarios here are
    /// written as "every block that needs a body carries its own settlement transaction", so the producer turns
    /// settlement batching off (D-077); [`Side::batched`] turns it on.
    pub fn new(mock: &Arc<MockBtc>, genesis: &GenesisFile, name: &str) -> Side {
        Side::with_params(mock, genesis, name, DEVNET)
    }

    /// Same as [`Side::new`], with the producer's settlement batching on (D-077).
    pub fn batched(mock: &Arc<MockBtc>, genesis: &GenesisFile, name: &str) -> Side {
        Side::open(mock, genesis, name, DEVNET, true)
    }

    /// Same as [`Side::new`], but the producer has no funding wallet: its blocks carry no settlement transaction and
    /// their bodies are carried onto BTC by someone else (D-095).
    pub fn unfunded(mock: &Arc<MockBtc>, genesis: &GenesisFile, name: &str) -> Side {
        Side::open_with_funding(
            mock,
            genesis,
            name,
            DEVNET,
            false,
            false,
            Pruning::default(),
        )
    }

    /// With the given params and pruning settings (D-097).
    pub fn pruned(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        params: Params,
        pruning: Pruning,
    ) -> Side {
        Side::open_with_funding(mock, genesis, name, params, false, true, pruning)
    }

    pub fn with_params(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        params: Params,
    ) -> Side {
        Side::open(mock, genesis, name, params, false)
    }

    fn open(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        params: Params,
        batching: bool,
    ) -> Side {
        Side::open_with_funding(
            mock,
            genesis,
            name,
            params,
            batching,
            true,
            Pruning::default(),
        )
    }

    fn open_with_funding(
        mock: &Arc<MockBtc>,
        genesis: &GenesisFile,
        name: &str,
        params: Params,
        batching: bool,
        funded: bool,
        pruning: Pruning,
    ) -> Side {
        let dir = tempfile::tempdir().unwrap();
        let options = ChainOptions {
            pruning,
            ..ChainOptions::default()
        };
        let chain = Arc::new(
            Chain::open_with(dir.path(), params.clone(), genesis, mock.clone(), options).unwrap(),
        );
        while chain.sync_btc().unwrap() {}
        let funding = funded.then(|| {
            let secret =
                SecretKey::from_slice(&sha256(format!("m4/{name}/funding").as_bytes())).unwrap();
            let funding = Funding::new(Keypair::from_secret_key(&Secp256k1::new(), &secret), 2)
                .with_batching(batching);
            mock.fund(&funding.script, &[400_000; 4]);
            funding
        });
        while chain.sync_btc().unwrap() {}
        let mempool = Arc::new(Mutex::new(Mempool::new(params, Policy::default())));
        let producer = Producer {
            chain: chain.clone(),
            mempool: mempool.clone(),
            producer: user(&format!("m4/{name}/producer")).account,
            funding,
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
            dir,
        }
    }

    /// Sync with the mock chain (a real node syncs periodically on a background thread). Sync with the mock BTC chain
    /// until nothing changes, and, like the node, maintain the entry pool after the main chain changes as a result
    /// (D-041, D-082).
    pub fn sync(&self) {
        while self.chain.sync_btc().unwrap() {}
        ksana_node::ops::apply_chain_changes(&self.chain, &self.mempool, &self.producer.events)
            .unwrap();
    }

    /// Accept a block (syncing BTC first), and maintain the entry pool like the node does (D-041: the ops of
    /// disconnected blocks go back into the entry pool).
    pub fn accept(&self, block: &Block) -> Result<AcceptReport, NodeError> {
        self.sync();
        let report = self.chain.accept_block(block.clone(), None)?;
        ksana_node::ops::chain_changed(
            &self.chain,
            &self.mempool,
            &self.producer.events,
            &report.connected,
            &report.disconnected,
        )?;
        Ok(report)
    }

    /// Produce one block (retrying until it succeeds if necessary).
    pub fn mine(&self) -> Block {
        self.sync();
        for _ in 0..20 {
            if let Some(block) = self.producer.step().unwrap() {
                return block;
            }
        }
        panic!("block production failed");
    }

    pub fn submit(&self, op: VmOp) {
        let (_, view) = self.chain.tip_and_view().unwrap();
        let state: Arc<dyn StateSource> = Arc::new(view);
        self.mempool.lock().unwrap().insert(op, &state).unwrap();
    }

    /// Whether the op is in the entry pool.
    pub fn pending(&self, op: &VmOp) -> bool {
        self.mempool.lock().unwrap().contains(&op.op_id().unwrap())
    }

    pub fn ksa(&self, u: &User) -> u128 {
        self.balance(&u.account)
    }

    pub fn balance(&self, account: &[u8; 32]) -> u128 {
        self.chain
            .with_state(|state, _| {
                state
                    .get(&keys::balance(&KSA_ASSET_ID, account))
                    .unwrap()
                    .map(|v| borsh::from_slice::<u128>(&v).unwrap())
                    .unwrap_or(0)
            })
            .unwrap()
    }

    pub fn nonce(&self, u: &User) -> u64 {
        self.chain
            .with_state(|state, _| {
                state
                    .get(&keys::account(&u.account))
                    .unwrap()
                    .map(|v| borsh::from_slice::<AccountState>(&v).unwrap().nonce)
                    .unwrap_or(0)
            })
            .unwrap()
    }
}

/// A mock BTC chain (1 block) and a genesis config anchored at its tip.
pub fn setup(allocations: &[(&User, u128)]) -> (Arc<MockBtc>, GenesisFile) {
    setup_with_share(allocations, None)
}

/// Same as [`setup`], with the genesis config having a block share account (D-089).
pub fn setup_with_share(
    allocations: &[(&User, u128)],
    block_share_account: Option<[u8; 32]>,
) -> (Arc<MockBtc>, GenesisFile) {
    let mock = Arc::new(MockBtc::new());
    mock.mine(1);
    let (height, hash) = mock.tip().unwrap();
    let genesis = GenesisFile {
        timestamp: genesis_timestamp(),
        btc_anchor: BtcAnchor { height, hash },
        bits: None,
        block_share_account,
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
