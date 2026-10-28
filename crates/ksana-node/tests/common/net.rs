//! Multi-node test fixture: in-process nodes interconnected on the local machine over a real libp2p network, sharing
//! one in-memory mock BTC chain `MockBtc`; each node reaches the mock chain through a [`Recorder`] that records the
//! transactions it broadcast. Shared by the M4 and M5 node tests.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::Transaction;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use ksana_btc::client::{BtcClient, BtcError, HeaderInfo, ScannedUtxo, TxInfo, UtxoInfo};
use ksana_btc::mock::MockBtc;
use ksana_consensus::PowAlgorithm;
use ksana_execution::keys::{self, AccountState};
use ksana_mempool::Policy;
use ksana_node::chain::Tip;
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::net::Network;
use ksana_node::node::{NetConfig, Node, NodeConfig};
use ksana_node::producer::Funding;
use ksana_p2p::{Multiaddr, PeerId, Tuning};
use ksana_primitives::hash::sha256;
use ksana_primitives::{BtcAnchor, Params, VmOp, DEVNET, KSA_ASSET_ID};
use ksana_rpc::{KsanaApiClient, OpStatusJson, SettlementJson, SettlementState};

use super::{genesis_timestamp, user, User};

/// A BTC client that records the transactions broadcast through it (one per node, all sharing the same mock chain).
pub struct Recorder {
    pub mock: Arc<MockBtc>,
    pub broadcasts: Mutex<Vec<Transaction>>,
}

impl BtcClient for Recorder {
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
        self.mock.test_accept(txs)
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        self.broadcasts.lock().unwrap().push(tx.clone());
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

pub struct TestNode {
    pub node: Option<Node>,
    pub client: HttpClient,
    pub rt: tokio::runtime::Runtime,
    pub recorder: Arc<Recorder>,
    pub peer_id: PeerId,
    /// The listen address with /p2p/<peer id>.
    pub addr: Multiaddr,
    pub _dir: tempfile::TempDir,
}

impl TestNode {
    pub fn node(&self) -> &Node {
        self.node.as_ref().unwrap()
    }

    pub fn net(&self) -> &Network {
        self.node().network().unwrap()
    }

    pub fn tip(&self) -> Tip {
        self.node().chain().tip().unwrap()
    }

    pub fn height(&self) -> u64 {
        self.tip().header.height
    }

    pub fn pause(&self, paused: bool) {
        self.node().set_paused(paused);
    }

    pub fn submit(&self, op: &VmOp) -> [u8; 32] {
        self.rt
            .block_on(self.client.submit_op(hex::encode(op.encode().unwrap())))
            .unwrap()
            .op_id
    }

    /// The op status; `None` when the node does not know the op.
    pub fn status(&self, op_id: &[u8; 32]) -> Option<OpStatusJson> {
        self.rt
            .block_on(self.client.get_op_status(hex::encode(op_id)))
            .ok()
    }

    /// Query the settlement by block hash (each merged block carries its own settlement transaction, D-063).
    pub fn settlement_of(&self, block: &[u8; 32]) -> SettlementJson {
        self.rt
            .block_on(self.client.get_settlement(hex::encode(block)))
            .unwrap()
    }

    pub fn settlement(&self, height: u64) -> SettlementJson {
        self.rt
            .block_on(self.client.get_settlement(height.to_string()))
            .unwrap()
    }

    /// The main-chain tip and protocol version (`ksana_getChainInfo`, D-076).
    pub fn chain_info(&self) -> ksana_rpc::ChainInfoJson {
        self.rt.block_on(self.client.get_chain_info()).unwrap()
    }

    pub fn state<T: borsh::BorshDeserialize>(&self, key: &[u8]) -> Option<T> {
        self.node()
            .chain()
            .with_state(|state, _| {
                state
                    .get(key)
                    .unwrap()
                    .map(|v| borsh::from_slice::<T>(&v).unwrap())
            })
            .unwrap()
    }

    pub fn ksa(&self, u: &User) -> u128 {
        self.state(&keys::balance(&KSA_ASSET_ID, &u.account))
            .unwrap_or(0)
    }

    pub fn nonce(&self, u: &User) -> u64 {
        self.state::<AccountState>(&keys::account(&u.account))
            .map_or(0, |a| a.nonce)
    }

    pub fn broadcasts(&self) -> Vec<Transaction> {
        self.recorder.broadcasts.lock().unwrap().clone()
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        if let Some(node) = self.node.take() {
            node.stop().unwrap();
        }
    }
}

pub struct Net {
    pub mock: Arc<MockBtc>,
    pub nodes: Vec<TestNode>,
    pub genesis: GenesisFile,
    pub interval: Duration,
    /// The mining algorithm the nodes use (`None` for the one chosen by the network params, D-062).
    pub pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
    /// The protocol params of each node (DEVNET when not given; in the upgrade tests, upgraded and non-upgraded nodes
    /// use different upgrade tables, D-076).
    pub params: BTreeMap<usize, Params>,
}

/// Network params for tests: shorter intervals for the periodic Status query and for redialing.
pub fn tuning() -> Tuning {
    Tuning {
        status_interval: Duration::from_millis(500),
        redial_interval: Duration::from_millis(300),
        request_timeout: Duration::from_secs(10),
        ..Tuning::default()
    }
}

/// The producers of the main-chain blocks at heights 1..=tip-`skip`.
pub fn producers(node: &TestNode, skip: u64) -> BTreeSet<[u8; 32]> {
    let chain = node.node().chain();
    let top = node.height().saturating_sub(skip);
    (1..=top)
        .map(|h| {
            let hash = chain.store().hash_at(h).unwrap().unwrap();
            chain.store().block(&hash).unwrap().unwrap().header.producer
        })
        .collect()
}

pub fn wait<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let started = ksana_node::monotonic_now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Net {
    /// Start `n` nodes (each with confirmed funding UTXOs), each bootstrapping from the nodes started before it; all
    /// pause block production after starting.
    pub fn start(n: usize, allocations: &[(&User, u128)], interval: Duration) -> Net {
        Net::start_custom(n, allocations, interval, None, None)
    }

    /// Same as [`Net::start`], with all nodes running with `params` (the D-076 upgrade tests).
    pub fn start_with_params(
        n: usize,
        allocations: &[(&User, u128)],
        interval: Duration,
        params: Params,
    ) -> Net {
        let params = (0..n).map(|i| (i, params.clone())).collect();
        Net::start_inner(n, allocations, interval, None, None, params)
    }

    /// Same as [`Net::start`], but the genesis difficulty (`bits`) and the mining algorithm (D-062) can also be given.
    pub fn start_custom(
        n: usize,
        allocations: &[(&User, u128)],
        interval: Duration,
        bits: Option<u32>,
        pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
    ) -> Net {
        Net::start_inner(
            n,
            allocations,
            interval,
            bits,
            pow_algorithm,
            BTreeMap::new(),
        )
    }

    fn start_inner(
        n: usize,
        allocations: &[(&User, u128)],
        interval: Duration,
        bits: Option<u32>,
        pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
        params: BTreeMap<usize, Params>,
    ) -> Net {
        let mock = Arc::new(MockBtc::new());
        mock.mine(1);
        let secp = Secp256k1::new();
        let fundings: Vec<Funding> = (0..n)
            .map(|i| {
                let secret =
                    SecretKey::from_slice(&sha256(format!("m4/net/funding/{i}").as_bytes()))
                        .unwrap();
                let funding = Funding::new(Keypair::from_secret_key(&secp, &secret), 2);
                mock.fund(&funding.script, &[500_000; 8]);
                funding
            })
            .collect();
        let (height, hash) = mock.tip().unwrap();
        let genesis = GenesisFile {
            // Not moved early when a genesis difficulty is given: the difficulty adjustment has to start from it (when
            // behind schedule the target would rise to the PoW limit and the genesis difficulty would be meaningless)
            timestamp: if bits.is_some() {
                ksana_node::unix_now()
            } else {
                genesis_timestamp()
            },
            btc_anchor: BtcAnchor { height, hash },
            bits,
            block_share_account: None,
            allocations: allocations
                .iter()
                .map(|(u, amount)| Allocation {
                    account: u.account,
                    amount: *amount,
                })
                .collect(),
        };
        let mut net = Net {
            mock,
            nodes: Vec::new(),
            genesis,
            interval,
            pow_algorithm,
            params,
        };
        for (i, funding) in fundings.into_iter().enumerate() {
            let node = net.spawn(i, Some(funding));
            net.nodes.push(node);
        }
        net.wait_connected(&(0..n).collect::<Vec<_>>());
        net
    }

    /// Add a node later (non-producing, bootstrapping from all existing nodes), syncing from the genesis block. Returns
    /// its index.
    pub fn join(&mut self) -> usize {
        let i = self.nodes.len();
        let node = self.spawn(i, None);
        self.nodes.push(node);
        i
    }

    /// Same as [`Net::join`], with the new node running with `params` (e.g. non-upgraded software, D-076).
    pub fn join_with(&mut self, params: Params) -> usize {
        self.params.insert(self.nodes.len(), params);
        self.join()
    }

    /// Stop the non-producing node `i` and restart it with `params` using the same data directory and P2P key
    /// (switching to another software version, D-076), then wait for it to reconnect with the other nodes.
    pub fn restart(&mut self, i: usize, params: Params) {
        let node = self.nodes[i].node.take().expect("the node is running");
        node.stop().unwrap();
        let dir = std::mem::replace(&mut self.nodes[i]._dir, tempfile::tempdir().unwrap());
        self.params.insert(i, params);
        self.nodes[i] = self.spawn_in(i, None, dir);
        self.wait_connected(&(0..self.nodes.len()).collect::<Vec<_>>());
    }

    /// Start node `i` (block production paused after starting); with `funding` of `None` it does not produce blocks.
    fn spawn(&self, i: usize, funding: Option<Funding>) -> TestNode {
        self.spawn_in(i, funding, tempfile::tempdir().unwrap())
    }

    /// Same as [`Net::spawn`], with the data kept in `dir` (a restart keeps the original directory).
    fn spawn_in(&self, i: usize, funding: Option<Funding>, dir: tempfile::TempDir) -> TestNode {
        let recorder = Arc::new(Recorder {
            mock: self.mock.clone(),
            broadcasts: Mutex::default(),
        });
        let keypair =
            ksana_p2p::Keypair::ed25519_from_bytes(sha256(format!("m4/net/p2p/{i}").as_bytes()))
                .unwrap();
        let producer = funding
            .as_ref()
            .map(|_| user(&format!("m4/net/producer/{i}")).account);
        let node = Node::start(NodeConfig {
            data_dir: dir.path().join("data"),
            genesis: self.genesis.clone(),
            params: self.params.get(&i).cloned().unwrap_or(DEVNET),
            rpc_addr: "127.0.0.1:0".parse().unwrap(),

            rpc_cors: Default::default(),
            producer,
            carrier_payee: None,
            funding,
            policy: Policy::default(),
            btc: recorder.clone(),
            btc_poll: Duration::from_millis(50),
            min_block_interval: self.interval,
            net: Some(NetConfig {
                keypair,
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                peers: self
                    .nodes
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, n)| n.addr.clone())
                    .collect(),
                tuning: tuning(),
            }),
            pow_algorithm: self.pow_algorithm.clone(),
            pruning: ksana_node::chain::Pruning::default(),
        })
        .unwrap();
        node.set_paused(true);
        let network = node.network().unwrap();
        let peer_id = network.local_peer_id();
        let addr: Multiaddr = format!("{}/p2p/{peer_id}", network.listen_addrs()[0])
            .parse()
            .unwrap();
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(format!("http://{}", node.rpc_addr()))
            .unwrap();
        TestNode {
            node: Some(node),
            client,
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
            recorder,
            peer_id,
            addr,
            _dir: dir,
        }
    }

    /// Wait until the nodes in `group` are all connected to each other and have all subscribed to the block topic.
    pub fn wait_connected(&self, group: &[usize]) {
        wait("the nodes to interconnect", Duration::from_secs(30), || {
            group
                .iter()
                .all(|&i| {
                    let me = &self.nodes[i];
                    let connected = me.net().connected_peers();
                    let gossip = me.net().gossip_peers();
                    group.iter().filter(|&&j| j != i).all(|&j| {
                        let peer = self.nodes[j].peer_id;
                        connected.contains(&peer) && gossip.contains(&peer)
                    })
                })
                .then_some(())
        });
    }

    /// Isolate two groups of nodes (connections refused in both directions).
    pub fn partition(&self, left: &[usize], right: &[usize]) {
        for &l in left {
            for &r in right {
                self.nodes[l].net().block_peer(self.nodes[r].peer_id);
                self.nodes[r].net().block_peer(self.nodes[l].peer_id);
            }
        }
        wait(
            "the isolation to take effect",
            Duration::from_secs(30),
            || {
                left.iter()
                    .all(|&l| {
                        let connected = self.nodes[l].net().connected_peers();
                        right
                            .iter()
                            .all(|&r| !connected.contains(&self.nodes[r].peer_id))
                    })
                    .then_some(())
            },
        );
    }

    /// Lift the isolation and wait for all nodes to reconnect.
    pub fn heal(&self, left: &[usize], right: &[usize]) {
        for &l in left {
            for &r in right {
                self.nodes[l].net().unblock_peer(self.nodes[r].peer_id);
                self.nodes[r].net().unblock_peer(self.nodes[l].peer_id);
            }
        }
        self.wait_connected(&(0..self.nodes.len()).collect::<Vec<_>>());
    }

    /// Wait until all nodes have the same main-chain tip.
    pub fn converge(&self) -> Tip {
        wait(
            "the nodes to converge on the same tip",
            Duration::from_secs(60),
            || {
                let tip = self.nodes[0].tip();
                self.nodes
                    .iter()
                    .all(|n| n.tip().hash == tip.hash)
                    .then_some(tip)
            },
        )
    }

    /// Let only `i` produce blocks until its height has grown by `count`, then pause.
    pub fn mine_on(&self, i: usize, count: u64) {
        let node = &self.nodes[i];
        let target = node.height() + count;
        node.pause(false);
        wait("block production", Duration::from_secs(60), || {
            (node.height() >= target).then_some(())
        });
        node.pause(true);
    }
}

impl TestNode {
    pub fn wait_settlement(&self, height: u64, state: SettlementState) -> SettlementJson {
        wait("the settlement state", Duration::from_secs(30), || {
            let s = self.settlement(height);
            (s.status == state).then_some(s)
        })
    }
}
