//! M4 acceptance tests (SPEC §14 M4, `tests/integration/run.sh`): the devnet's regtest bitcoind + 3 in-process
//! `ksana-node`s connected to each other on this machine over a real libp2p network (the 3-node docker compose
//! deployment is in D-042, for manual runs and demos).
//!
//! 1. 3 nodes produce blocks concurrently at a high rate: parallel blocks are merged by later blocks, no orphan blocks
//!    arise, and the state converges (D-063).
//! 2. Partition the network to create a fork, then reconnect: the nodes switch to the chain with more blue work, later
//!    blocks merge the other side, and the ops of both sides execute (of conflicting ones only one).
//! 3. No conflicting settlement transactions are broadcast to BTC during the fork: both sides include the same path-B
//!    op and each carries a settlement transaction; the side with depth less than `SETTLEMENT_DEPTH` does not
//!    broadcast; after reconnecting, the lighter side's block is merged, its entries skipped, and the settlement
//!    transaction it carries lands on BTC as usual afterwards. Settlement transactions spend only the producer's own
//!    funding (D-078); on regtest no two transactions spend the same output.
//! 4. Parallel blocks on both sides of the partition include the same native transaction (overlapping
//!    forced-inclusion windows): after the merge it executes only once (D-063).
//!
//! Each test uses its own funding secret key, users and data directory; only the test itself mines BTC blocks. Run in
//! order with `--test-threads=1`.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::{Amount, BlockHash, OutPoint as BtcOutPoint, Transaction, TxOut};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use ksana_btc::client::{
    BtcClient, BtcError, HeaderInfo, RpcClient, ScannedUtxo, TxInfo, UtxoInfo,
};
use ksana_crypto::{account_of, keypair_from_secret, sign_native_vmop, sign_vmop};
use ksana_execution::keys::{self, AccountState};
use ksana_integration_tests::Btc;
use ksana_mempool::Policy;
use ksana_node::chain::Tip;
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::net::Network;
use ksana_node::node::{NetConfig, Node, NodeConfig};
use ksana_node::producer::Funding;
use ksana_p2p::{Multiaddr, PeerId, Tuning};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, BtcAnchor, Entry, FeeMode, OutPoint, SigScheme, VmOp, DEVNET, KSA_ASSET_ID,
    VMOP_VERSION,
};
use ksana_rpc::{KsanaApiClient, OpState, OpStatusJson, SettlementJson, SettlementState};
use serde_json::json;

struct User {
    kp: ksana_crypto::Keypair,
    account: [u8; 32],
}

fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(format!("ksana/it/m4/{seed}").as_bytes())).unwrap();
    User {
        account: account_of(&kp),
        kp,
    }
}

fn signed(u: &User, nonce: u64, fee: FeeMode, to: [u8; 32], amount: u128) -> VmOp {
    let mut op = VmOp {
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
    };
    op.sig = sign_vmop(&op, &u.kp, &[6; 32]).unwrap();
    op
}

/// A path-C KSA transfer op: the signature is bound to the carrying transaction's first input `first_input` (D-086).
fn signed_native(u: &User, nonce: u64, first_input: &OutPoint, to: [u8; 32], amount: u128) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::BtcNative,
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to,
            amount,
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_native_vmop(&op, first_input, &u.kp, &[6; 32]).unwrap();
    op
}

fn btc_secret(seed: &str) -> SecretKey {
    SecretKey::from_slice(&sha256(format!("ksana/it/m4/btc/{seed}").as_bytes())).unwrap()
}

fn btc_env() -> (String, String, String) {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    (
        env("KSANA_BTC_RPC_URL", "http://127.0.0.1:18443"),
        env("KSANA_BTC_RPC_USER", "ksana"),
        env("KSANA_BTC_RPC_PASS", "ksana"),
    )
}

/// The current regtest tip (hash in internal byte order).
fn btc_tip(btc: &Btc) -> BtcAnchor {
    let hash = btc.call("getbestblockhash", &[]);
    let hash = BlockHash::from_str(hash.as_str().unwrap()).unwrap();
    let height = btc.call("getblockcount", &[]).as_u64().unwrap();
    BtcAnchor {
        height: u32::try_from(height).unwrap(),
        hash: hash.to_byte_array(),
    }
}

/// A transaction funded by the wallet that pays `outputs`, broadcast; returns each output's outpoint.
fn pay(btc: &Btc, outputs: Vec<TxOut>) -> Vec<BtcOutPoint> {
    let tx = btc.fund_and_sign(outputs.clone());
    btc.send(&tx);
    let txid = tx.compute_txid();
    outputs
        .iter()
        .map(|o| {
            let vout = tx.output.iter().position(|x| x == o).unwrap();
            BtcOutPoint::new(txid, u32::try_from(vout).unwrap())
        })
        .collect()
}

fn mine(btc: &Btc, n: u64) {
    let addr = btc.new_address();
    btc.mine(n, &addr);
}

/// A BTC client that records the transactions broadcast through it (one per node).
struct Recorder {
    inner: RpcClient,
    broadcasts: Mutex<Vec<Transaction>>,
}

impl BtcClient for Recorder {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        self.inner.tip()
    }

    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        self.inner.block_hash(height)
    }

    fn block(&self, hash: &[u8; 32]) -> Result<Option<bitcoin::Block>, BtcError> {
        self.inner.block(hash)
    }

    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        self.inner.header(hash)
    }

    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        self.inner.transaction(txid)
    }

    fn utxo(
        &self,
        outpoint: &BtcOutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        self.inner.utxo(outpoint, include_mempool)
    }

    fn scan_utxos(&self, script: &bitcoin::Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        self.inner.scan_utxos(script)
    }

    fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        self.inner.test_accept(txs)
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        self.broadcasts.lock().unwrap().push(tx.clone());
        self.inner.broadcast(tx)
    }

    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        self.inner.in_mempool(txid)
    }

    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        self.inner.mempool_txids()
    }

    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        self.inner.txindex_height()
    }
}

struct TestNode {
    node: Option<Node>,
    client: HttpClient,
    rt: tokio::runtime::Runtime,
    recorder: Arc<Recorder>,
    peer_id: PeerId,
    /// The listen address with /p2p/<peer id>.
    addr: Multiaddr,
    _dir: tempfile::TempDir,
}

impl TestNode {
    fn node(&self) -> &Node {
        self.node.as_ref().unwrap()
    }

    fn net(&self) -> &Network {
        self.node().network().unwrap()
    }

    fn tip(&self) -> Tip {
        self.node().chain().tip().unwrap()
    }

    fn height(&self) -> u64 {
        self.tip().header.height
    }

    fn pause(&self, paused: bool) {
        self.node().set_paused(paused);
    }

    fn submit(&self, op: &VmOp) -> [u8; 32] {
        self.rt
            .block_on(self.client.submit_op(hex::encode(op.encode().unwrap())))
            .unwrap()
            .op_id
    }

    /// The op's status; `None` when the node does not know the op.
    fn status(&self, op_id: &[u8; 32]) -> Option<OpStatusJson> {
        self.rt
            .block_on(self.client.get_op_status(hex::encode(op_id)))
            .ok()
    }

    fn settlement(&self, height: u64) -> SettlementJson {
        self.rt
            .block_on(self.client.get_settlement(height.to_string()))
            .unwrap()
    }

    fn wait_settlement(&self, height: u64, state: SettlementState) -> SettlementJson {
        wait("the settlement state", Duration::from_secs(60), || {
            let s = self.settlement(height);
            (s.status == state).then_some(s)
        })
    }

    fn state<T: borsh::BorshDeserialize>(&self, key: &[u8]) -> Option<T> {
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

    fn ksa(&self, u: &User) -> u128 {
        self.state(&keys::balance(&KSA_ASSET_ID, &u.account))
            .unwrap_or(0)
    }

    fn nonce(&self, u: &User) -> u64 {
        self.state::<AccountState>(&keys::account(&u.account))
            .map_or(0, |a| a.nonce)
    }

    fn broadcasts(&self) -> Vec<Transaction> {
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

/// The producers of the main-chain blocks at heights 1..=tip−`skip`.
fn producers(node: &TestNode, skip: u64) -> BTreeSet<[u8; 32]> {
    let chain = node.node().chain();
    let top = node.height().saturating_sub(skip);
    (1..=top)
        .map(|h| {
            let hash = chain.store().hash_at(h).unwrap().unwrap();
            chain.store().block(&hash).unwrap().unwrap().header.producer
        })
        .collect()
}

fn wait<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let started = ksana_node::monotonic_now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Network tuning for tests: shorter intervals for the periodic Status query and redialing.
fn tuning() -> Tuning {
    Tuning {
        status_interval: Duration::from_millis(500),
        redial_interval: Duration::from_millis(300),
        request_timeout: Duration::from_secs(10),
        ..Tuning::default()
    }
}

struct Net {
    nodes: Vec<TestNode>,
}

impl Net {
    /// Starts 3 nodes (each funding wallet has 4 confirmed UTXOs; the genesis anchor is the current BTC tip), each
    /// using the previously started nodes as bootstrap peers; all of them pause block production after starting.
    fn start(btc: &Btc, name: &str, allocations: &[(&User, u128)], interval: Duration) -> Net {
        let secp = Secp256k1::new();
        let fundings: Vec<Funding> = (0..3)
            .map(|i| {
                let keypair =
                    Keypair::from_secret_key(&secp, &btc_secret(&format!("{name}/funding/{i}")));
                Funding::new(keypair, 2)
            })
            .collect();
        let outputs = fundings
            .iter()
            .flat_map(|f| {
                (0..4).map(|_| TxOut {
                    value: Amount::from_sat(200_000),
                    script_pubkey: f.script.clone(),
                })
            })
            .collect();
        pay(btc, outputs);
        mine(btc, 1);
        let genesis = GenesisFile {
            timestamp: ksana_node::unix_now(),
            btc_anchor: btc_tip(btc),
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
        let (url, rpc_user, rpc_pass) = btc_env();
        let mut nodes: Vec<TestNode> = Vec::new();
        for (i, funding) in fundings.into_iter().enumerate() {
            let recorder = Arc::new(Recorder {
                inner: RpcClient::new(&url, &rpc_user, &rpc_pass).unwrap(),
                broadcasts: Mutex::default(),
            });
            let dir = tempfile::tempdir().unwrap();
            let keypair = ksana_p2p::Keypair::ed25519_from_bytes(sha256(
                format!("ksana/it/m4/{name}/p2p/{i}").as_bytes(),
            ))
            .unwrap();
            let node = Node::start(NodeConfig {
                data_dir: dir.path().join("data"),
                genesis: genesis.clone(),
                params: DEVNET,
                rpc_addr: "127.0.0.1:0".parse().unwrap(),

                rpc_cors: Default::default(),
                producer: Some(user(&format!("{name}/producer/{i}")).account),
                carrier_payee: None,
                funding: Some(funding),
                policy: Policy::default(),
                btc: recorder.clone(),
                btc_poll: Duration::from_millis(100),
                min_block_interval: interval,
                net: Some(NetConfig {
                    keypair,
                    listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                    peers: nodes.iter().map(|n| n.addr.clone()).collect(),
                    tuning: tuning(),
                }),
                pow_algorithm: None,
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
                .request_timeout(Duration::from_secs(60))
                .build(format!("http://{}", node.rpc_addr()))
                .unwrap();
            nodes.push(TestNode {
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
            });
        }
        let net = Net { nodes };
        net.wait_connected(&[0, 1, 2]);
        net
    }

    fn wait_connected(&self, group: &[usize]) {
        wait(
            "the nodes to connect to each other",
            Duration::from_secs(30),
            || {
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
            },
        );
    }

    fn partition(&self, left: &[usize], right: &[usize]) {
        for &l in left {
            for &r in right {
                self.nodes[l].net().block_peer(self.nodes[r].peer_id);
                self.nodes[r].net().block_peer(self.nodes[l].peer_id);
            }
        }
        wait(
            "the partition to take effect",
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

    fn heal(&self, left: &[usize], right: &[usize]) {
        for &l in left {
            for &r in right {
                self.nodes[l].net().unblock_peer(self.nodes[r].peer_id);
                self.nodes[r].net().unblock_peer(self.nodes[l].peer_id);
            }
        }
        self.wait_connected(&[0, 1, 2]);
    }

    fn converge(&self) -> Tip {
        wait(
            "all nodes to converge on the same tip",
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

    fn mine_on(&self, i: usize, count: u64) {
        let node = &self.nodes[i];
        let target = node.height() + count;
        node.pause(false);
        wait("block production", Duration::from_secs(60), || {
            (node.height() >= target).then_some(())
        });
        node.pause(true);
    }
}

/// No orphan blocks (D-063): 3 nodes produce blocks concurrently at a high rate (BTC is producing blocks too) and
/// parallel blocks appear frequently; every produced block is merged by a later block and enters the past of the
/// main-chain tip, and the nodes' DAGs, main chains and states agree.
#[test]
fn three_nodes_mine_concurrently_and_converge() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m4");
    let (alice, bob) = (user("converge/alice"), user("converge/bob"));
    let net = Net::start(
        &btc,
        "converge",
        &[(&alice, 1_000_000)],
        Duration::from_millis(100),
    );
    for node in &net.nodes {
        node.pause(false);
    }
    let ops: Vec<[u8; 32]> = net
        .nodes
        .iter()
        .zip(0u64..)
        .map(|(node, nonce)| {
            node.submit(&signed(
                &alice,
                nonce,
                FeeMode::Token { amount: 2 },
                bob.account,
                10,
            ))
        })
        .collect();
    let start = net.nodes.iter().map(TestNode::height).max().unwrap();
    // BTC is producing blocks meanwhile: the nodes' anchors follow the BTC tip
    for _ in 0..3 {
        std::thread::sleep(Duration::from_secs(2));
        mine(&btc, 1);
    }
    // The three nodes produce blocks at the same time until every node's blocks have entered the main chain (not
    // counting the latest 3 blocks) and parallel blocks have appeared
    wait(
        "the three nodes to produce blocks concurrently",
        Duration::from_secs(180),
        || {
            let high = net.nodes.iter().all(|n| n.height() >= start + 15);
            let parallel = net.nodes[0]
                .node()
                .chain()
                .main_chain()
                .unwrap()
                .entries()
                .iter()
                .any(|e| !e.merged.is_empty());
            (high && parallel && producers(&net.nodes[0], 3).len() == 3).then_some(())
        },
    );
    // Stop B and C; A keeps producing blocks until its block has merged all parallel blocks (each node's DAG has a
    // single tip left)
    net.nodes[1].pause(true);
    net.nodes[2].pause(true);
    let mut tip = None;
    for _ in 0..5 {
        net.mine_on(0, 1);
        let t = net.converge();
        if net
            .nodes
            .iter()
            .all(|n| n.node().chain().dag_tips().unwrap() == vec![t.hash])
        {
            tip = Some(t);
            break;
        }
    }
    let tip = tip.expect("all parallel blocks merged");
    let dag_len = net.nodes[0].node().chain().dag_len().unwrap();
    for node in &net.nodes {
        let chain = node.node().chain();
        assert_eq!(node.tip(), tip);
        assert_eq!(chain.dag_len().unwrap(), dag_len);
        // Every block is accepted by the main chain: parallel blocks are merged instead of becoming orphans
        for hash in chain.store().dag_order().unwrap() {
            assert!(chain.accepted_height(&hash).unwrap().is_some());
        }
        assert_eq!(node.ksa(&bob), 30);
        assert_eq!(node.nonce(&alice), 3);
        for op_id in &ops {
            assert_eq!(node.status(op_id).unwrap().status, OpState::FastConfirmed);
        }
    }
    for node in &net.nodes {
        assert_eq!(producers(node, 0).len(), 3);
    }
    assert!(tip.header.btc_anchor.height > net.nodes[0].node().chain().genesis().btc_anchor.height);
}

/// Reconnect after a partition (D-063): A syncs to B's chain, which has more blue work (the state is rolled back to
/// the fork point and re-executed along B's chain); B obtains A's former branch via the DAG tips in Status, and its
/// next block merges that branch: the ops of both sides execute, and of conflicting ops with the same nonce only the
/// one on the main chain executes.
#[test]
fn partition_then_reconnect_merges_both_sides() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m4");
    let (alice, bob, carol, dave, erin) = (
        user("partition/alice"),
        user("partition/bob"),
        user("partition/carol"),
        user("partition/dave"),
        user("partition/erin"),
    );
    let net = Net::start(
        &btc,
        "partition",
        &[(&alice, 1_000), (&dave, 1_000)],
        Duration::from_millis(300),
    );
    net.mine_on(0, 2);
    let fork = net.converge();

    net.partition(&[0], &[1, 2]);
    let to_bob = net.nodes[0].submit(&signed(
        &alice,
        0,
        FeeMode::Token { amount: 3 },
        bob.account,
        10,
    ));
    let to_erin = net.nodes[0].submit(&signed(
        &dave,
        0,
        FeeMode::Token { amount: 2 },
        erin.account,
        5,
    ));
    let to_carol = net.nodes[1].submit(&signed(
        &alice,
        0,
        FeeMode::Token { amount: 4 },
        carol.account,
        20,
    ));
    net.mine_on(0, 2);
    net.mine_on(1, 4);
    let (a, b, c) = (&net.nodes[0], &net.nodes[1], &net.nodes[2]);
    wait("C to catch up with B", Duration::from_secs(30), || {
        (c.tip().hash == b.tip().hash).then_some(())
    });
    assert_eq!(a.height(), fork.header.height + 2);
    assert_eq!(b.height(), fork.header.height + 4);
    assert_eq!(a.ksa(&bob), 10);
    assert_eq!(a.ksa(&erin), 5);
    assert_eq!(b.ksa(&carol), 20);
    let a_blocks: Vec<[u8; 32]> = (fork.header.height + 1..=a.height())
        .map(|h| a.node().chain().store().hash_at(h).unwrap().unwrap())
        .collect();

    net.heal(&[0], &[1, 2]);
    let tip = net.converge();
    assert_eq!(tip, b.tip());
    assert_eq!(a.ksa(&bob), 0);
    assert_eq!(a.ksa(&carol), 20);
    for (h, hash) in (fork.header.height + 1..).zip(&a_blocks) {
        assert_ne!(
            a.node().chain().store().hash_at(h).unwrap().as_ref(),
            Some(hash)
        );
    }
    // B and C obtain A's former branch via the DAG tips in Status; B's next block merges it
    wait(
        "B and C to obtain A's former branch",
        Duration::from_secs(30),
        || {
            net.nodes
                .iter()
                .all(|n| {
                    a_blocks
                        .iter()
                        .all(|h| n.node().chain().contains(h).unwrap())
                })
                .then_some(())
        },
    );
    net.mine_on(1, 1);
    let next = net.converge();
    assert_eq!(next.header.parents, vec![tip.hash, a_blocks[1]]);
    for node in &net.nodes {
        let chain = node.node().chain();
        for hash in &a_blocks {
            assert_eq!(
                chain.accepted_height(hash).unwrap(),
                Some(next.header.height)
            );
        }
        assert_eq!(node.ksa(&carol), 20);
        assert_eq!(node.ksa(&bob), 0);
        assert_eq!(node.ksa(&erin), 5);
        assert_eq!(node.ksa(&alice), 1_000 - 20 - 4);
        assert_eq!(node.ksa(&dave), 1_000 - 5 - 2);
        assert_eq!(node.nonce(&alice), 1);
        assert_eq!(
            node.status(&to_carol).unwrap().status,
            OpState::FastConfirmed
        );
        assert_eq!(
            node.status(&to_erin).unwrap().status,
            OpState::FastConfirmed
        );
        let skipped = chain.store().op_location(&to_bob).unwrap().unwrap();
        assert!(
            skipped.skipped,
            "the merged conflicting transfer is skipped"
        );
    }
}

/// No conflicting settlement transactions are broadcast during a fork (D-063, D-078): the same path-B op is included
/// by parallel blocks on both sides of the partition, each side's block carries a settlement transaction, and the
/// lighter side lacks depth and does not broadcast. After reconnecting, its block is merged (its entries skipped, the
/// op executes only once); once the merging block has `SETTLEMENT_DEPTH`, the settlement transaction it carries is
/// broadcast and lands on BTC as usual. Settlement transactions spend only the producer's own funding, and the inputs
/// occupied by an unconfirmed settlement are not reused by the same node: on regtest no two transactions spend the
/// same output.
#[test]
fn no_conflicting_settlements_are_broadcast_during_a_fork() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m4");
    let (alice, bob) = (user("settle/alice"), user("settle/bob"));
    let net = Net::start(
        &btc,
        "settle",
        &[(&alice, 1_000)],
        Duration::from_millis(600),
    );
    net.mine_on(0, 2);
    let fork = net.converge();
    let (a, b) = (&net.nodes[0], &net.nodes[1]);

    let op_id = a.submit(&signed(
        &alice,
        0,
        FeeMode::Token { amount: 3 },
        bob.account,
        7,
    ));
    wait("the op to reach all nodes", Duration::from_secs(30), || {
        net.nodes
            .iter()
            .all(|n| {
                n.status(&op_id)
                    .is_some_and(|s| s.status == OpState::Pending)
            })
            .then_some(())
    });

    net.partition(&[0], &[1, 2]);
    net.mine_on(0, 2);
    let a_height = a.status(&op_id).unwrap().block.unwrap().height;
    assert_eq!(a_height, fork.header.height + 1);
    let a_block = a.node().chain().store().hash_at(a_height).unwrap().unwrap();
    // B's side is heavier; it broadcasts the settlement transaction once its block has SETTLEMENT_DEPTH
    // confirmations; A's side lacks depth and does not broadcast
    net.mine_on(1, DEVNET.settlement_depth + 1);
    let b_height = b.status(&op_id).unwrap().block.unwrap().height;
    assert_eq!(b_height, fork.header.height + 1);
    let a_settlement = a.settlement(a_height);
    let b_settlement = b.settlement(b_height);
    assert_ne!(a_settlement.txid, b_settlement.txid);
    b.wait_settlement(b_height, SettlementState::Broadcast);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(a.settlement(a_height).status, SettlementState::Pending);
    // Only B's settlement transaction is in bitcoind's mempool
    let b_txid = bitcoin::Txid::from_byte_array(
        b_settlement
            .txid
            .expect("the block carries settlement transactions"),
    );
    let a_txid = bitcoin::Txid::from_byte_array(
        a_settlement
            .txid
            .expect("the block carries settlement transactions"),
    );
    assert!(btc
        .try_call("getmempoolentry", &[json!(b_txid.to_string())])
        .is_ok());
    assert!(btc
        .try_call("getrawtransaction", &[json!(a_txid.to_string())])
        .is_err());

    net.heal(&[0], &[1, 2]);
    let tip = net.converge();
    assert_eq!(tip, b.tip());
    mine(&btc, 1);
    for node in &net.nodes {
        let s = node.wait_settlement(b_height, SettlementState::Confirmed);
        assert_eq!(Some(s.confirmed.unwrap().txid), b_settlement.txid);
    }

    // B obtains A's former branch via the DAG tips in Status; its next block merges that branch, with its entries
    // skipped (D-063)
    wait(
        "B and C to obtain A's former branch",
        Duration::from_secs(30),
        || {
            net.nodes
                .iter()
                .all(|n| n.node().chain().contains(&a_block).unwrap())
                .then_some(())
        },
    );
    net.mine_on(1, 1);
    let merging = net.converge();
    for node in &net.nodes {
        assert_eq!(
            node.node().chain().accepted_height(&a_block).unwrap(),
            Some(merging.header.height)
        );
        assert_eq!(node.ksa(&bob), 7, "the op executes only once");
        assert_eq!(node.nonce(&alice), 1);
    }
    // Once the merging block has SETTLEMENT_DEPTH, the settlement transaction carried by A's block is broadcast as
    // usual (it spends only A's own funding and is still valid); after landing it counts as that block's data landing
    // on BTC (D-077)
    net.mine_on(1, DEVNET.settlement_depth);
    net.converge();
    wait(
        "the settlement transaction of A's block to be broadcast",
        Duration::from_secs(60),
        || {
            btc.try_call("getrawtransaction", &[json!(a_txid.to_string())])
                .is_ok()
                .then_some(())
        },
    );
    mine(&btc, 1);
    wait(
        "the settlement transaction of A's block to confirm",
        Duration::from_secs(60),
        || {
            net.nodes
                .iter()
                .all(|n| {
                    n.node()
                        .chain()
                        .btc()
                        .snapshot()
                        .unwrap()
                        .settlement_of(&a_block)
                        .is_some_and(|(txid, _)| Some(txid) == a_settlement.txid)
                })
                .then_some(())
        },
    );

    // No two of the transactions broadcast by the nodes spend the same output
    let mut spenders: BTreeMap<BtcOutPoint, BTreeSet<[u8; 32]>> = BTreeMap::new();
    for node in &net.nodes {
        for tx in node.broadcasts() {
            let txid = tx.compute_txid().to_byte_array();
            for input in &tx.input {
                spenders
                    .entry(input.previous_output)
                    .or_default()
                    .insert(txid);
            }
        }
    }
    for (outpoint, txids) in &spenders {
        assert_eq!(
            txids.len(),
            1,
            "{outpoint} is spent by several transactions: {txids:?}"
        );
    }
}

/// Forced inclusion (§7.4, D-063): parallel blocks on both sides of the partition include the same native transaction
/// (the inclusion windows of the two sides overlap); after reconnecting, the heavier side's block merges the other
/// side, and the native transaction executes only once, in BTC order.
#[test]
fn a_native_tx_in_parallel_blocks_executes_once() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m4");
    let (carol, dave) = (user("forced/carol"), user("forced/dave"));
    let net = Net::start(
        &btc,
        "forced",
        &[(&carol, 1_000)],
        Duration::from_millis(300),
    );
    net.mine_on(0, 2);
    let fork = net.converge();

    net.partition(&[0], &[1, 2]);
    // The native transaction confirms in a BTC block that both sides can see
    let (tx, op) = btc.fund_native(
        &DEVNET,
        &|input| signed_native(&carol, 0, input, dave.account, 9),
        Vec::new(),
    );
    btc.send(&tx);
    let op_id = op.op_id().unwrap();
    mine(&btc, 1);
    for node in &net.nodes {
        while node.node().chain().sync_btc().unwrap() {}
    }
    net.mine_on(0, 1);
    net.mine_on(1, 2);
    let (a, b, c) = (&net.nodes[0], &net.nodes[1], &net.nodes[2]);
    wait("C to catch up with B", Duration::from_secs(30), || {
        (c.tip().hash == b.tip().hash).then_some(())
    });
    let block_at = |node: &TestNode, height: u64| {
        let chain = node.node().chain();
        let hash = chain.store().hash_at(height).unwrap().unwrap();
        chain.store().block(&hash).unwrap().unwrap()
    };
    let forced = |block: &ksana_primitives::Block| {
        block
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Forced { op: o, .. } if *o == op))
    };
    let a_block = block_at(a, fork.header.height + 1);
    let b_block = block_at(b, fork.header.height + 1);
    assert!(
        forced(&a_block) && forced(&b_block),
        "the blocks on both sides include it"
    );
    assert_eq!(a.ksa(&dave), 9);
    assert_eq!(b.ksa(&dave), 9);

    // Reconnect: A switches to B's chain; once B has A's block, its next block merges it
    net.heal(&[0], &[1, 2]);
    let tip = net.converge();
    assert_eq!(tip, b.tip());
    let a_hash = a_block.header.hash();
    wait(
        "B and C to obtain A's block",
        Duration::from_secs(30),
        || {
            net.nodes
                .iter()
                .all(|n| n.node().chain().contains(&a_hash).unwrap())
                .then_some(())
        },
    );
    net.mine_on(1, 1);
    let merging = net.converge();
    for node in &net.nodes {
        let chain = node.node().chain();
        assert_eq!(
            chain.accepted_height(&a_hash).unwrap(),
            Some(merging.header.height)
        );
        // Executes only once: B's block is the one executing it; the copy in A's merged block does not change the state
        assert_eq!(node.ksa(&dave), 9);
        assert_eq!(node.nonce(&carol), 1);
        let status = node.status(&op_id).unwrap();
        assert_eq!(status.status, OpState::FastConfirmed);
        assert_eq!(status.block.unwrap().hash, b_block.header.hash());
    }
}
