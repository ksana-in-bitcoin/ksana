//! Fixtures of the M5 regtest tests (`tests/integration/run.sh`): the devnet's regtest bitcoind + an in-process
//! `ksana-node`. The single node [`Fast`] (taken from the M3 acceptance tests) and the 3 nodes [`Net`] connected via
//! libp2p (taken from the M4 acceptance tests).

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::{Amount, BlockHash, OutPoint as BtcOutPoint, ScriptBuf, Transaction, TxOut};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use ksana_btc::client::{
    BtcClient, BtcError, HeaderInfo, RpcClient, ScannedUtxo, TxInfo, UtxoInfo,
};
use ksana_consensus::PowAlgorithm;
use ksana_crypto::{account_of, keypair_from_secret, sign_native_vmop, sign_vmop};
use ksana_execution::keys::{self, AccountState};
use ksana_indexer::{Indexer, IndexerConfig, IndexerStatus};
use ksana_integration_tests::Btc;
use ksana_mempool::Policy;
use ksana_node::chain::Tip;
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::net::Network;
use ksana_node::node::{NetConfig, Node, NodeConfig};
use ksana_node::producer::{Funding, Revenue};
use ksana_p2p::{Multiaddr, PeerId, Tuning};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, BtcAnchor, FeeMode, OutPoint, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use ksana_rpc::{
    BlockJson, Cors, KsanaApiClient, OpState, OpStatusJson, SettlementJson, SettlementState,
};
use serde_json::{json, Value};

/// A user's VmOp account.
pub struct User {
    pub kp: ksana_crypto::Keypair,
    pub account: [u8; 32],
}

pub fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(format!("ksana/it/m5/{seed}").as_bytes())).unwrap();
    User {
        account: account_of(&kp),
        kp,
    }
}

/// A path-B KSA transfer op. For path C use [`signed_native`].
pub fn signed(u: &User, nonce: u64, fee: FeeMode, to: [u8; 32], amount: u128) -> VmOp {
    let mut op = unsigned_transfer(u, nonce, fee, to, amount);
    op.sig = sign_vmop(&op, &u.kp, &[7; 32]).unwrap();
    op
}

/// A path-C KSA transfer op: the signature is bound to the carrying transaction's first input `first_input` (D-086);
/// used together with `Btc::fund_native`.
pub fn signed_native(
    u: &User,
    nonce: u64,
    first_input: &OutPoint,
    to: [u8; 32],
    amount: u128,
) -> VmOp {
    let mut op = unsigned_transfer(u, nonce, FeeMode::BtcNative, to, amount);
    op.sig = sign_native_vmop(&op, first_input, &u.kp, &[7; 32]).unwrap();
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

/// Funds a path-C native transaction from the wallet and broadcasts it (`Btc::fund_native`), returning the
/// transaction and the op it carries.
pub fn pay_native(btc: &Btc, sign: &dyn Fn(&OutPoint) -> VmOp) -> (Transaction, VmOp) {
    let (tx, op) = btc.fund_native(&DEVNET, sign, Vec::new());
    btc.send(&tx);
    (tx, op)
}

/// A signed op with an arbitrary action.
pub fn signed_action(u: &User, nonce: u64, fee: FeeMode, action: Action, max_gas: u64) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee,
        max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[7; 32]).unwrap();
    op
}

/// A BTC secret key for tests (deterministic).
pub fn btc_secret(seed: &str) -> SecretKey {
    SecretKey::from_slice(&sha256(format!("ksana/it/m5/btc/{seed}").as_bytes())).unwrap()
}

pub fn btc_env() -> (String, String, String) {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    (
        env("KSANA_BTC_RPC_URL", "http://127.0.0.1:18443"),
        env("KSANA_BTC_RPC_USER", "ksana"),
        env("KSANA_BTC_RPC_PASS", "ksana"),
    )
}

/// The current regtest tip (hash in internal byte order).
pub fn btc_tip(btc: &Btc) -> BtcAnchor {
    let hash = btc.call("getbestblockhash", &[]);
    let hash = BlockHash::from_str(hash.as_str().unwrap()).unwrap();
    let height = btc.call("getblockcount", &[]).as_u64().unwrap();
    BtcAnchor {
        height: u32::try_from(height).unwrap(),
        hash: hash.to_byte_array(),
    }
}

/// A transaction funded by the wallet that pays `outputs`, broadcast; returns the transaction and each output's
/// outpoint.
pub fn pay(btc: &Btc, outputs: Vec<TxOut>) -> (Transaction, Vec<BtcOutPoint>) {
    let tx = btc.fund_and_sign(outputs.clone());
    btc.send(&tx);
    let txid = tx.compute_txid();
    let points = outputs
        .iter()
        .map(|o| {
            let vout = tx.output.iter().position(|x| x == o).unwrap();
            BtcOutPoint::new(txid, u32::try_from(vout).unwrap())
        })
        .collect();
    (tx, points)
}

/// Spends the output `point` at the funding address to a new wallet address with the producer's funding secret key
/// (BIP86 key-path) and broadcasts it (simulating the operator using this hot-wallet output elsewhere): the
/// settlement transaction spending it can no longer land on BTC (D-045). Returns this conflicting transaction.
pub fn spend_producer_input(btc: &Btc, keypair: &Keypair, point: BtcOutPoint) -> Transaction {
    use bitcoin::consensus::encode::deserialize_hex;
    use bitcoin::secp256k1::Message;
    use bitcoin::sighash::{Prevouts, SighashCache};
    use bitcoin::{absolute, transaction, Sequence, TapSighashType, TxIn, Witness};

    let prev = btc.call("getrawtransaction", &[json!(point.txid.to_string())]);
    let prev: Transaction = deserialize_hex(prev.as_str().unwrap()).unwrap();
    let txout = prev.output[usize::try_from(point.vout).unwrap()].clone();
    let info = btc.call("getaddressinfo", &[json!(btc.new_address())]);
    let to = ScriptBuf::from_bytes(hex::decode(info["scriptPubKey"].as_str().unwrap()).unwrap());
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: point,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: txout.value - Amount::from_sat(1_000),
            script_pubkey: to,
        }],
    };
    let secp = Secp256k1::new();
    let tweaked = ksana_btc::spend::bip86_keypair(&secp, keypair).unwrap();
    let sighash = SighashCache::new(&tx)
        .taproot_key_spend_signature_hash(0, &Prevouts::All(&[&txout]), TapSighashType::Default)
        .unwrap();
    let signature = secp.sign_schnorr_no_aux_rand(&Message::from(sighash), &tweaked);
    tx.input[0].witness = Witness::from_slice(&[signature.serialize()]);
    btc.send(&tx);
    tx
}

pub fn mine(btc: &Btc, n: u64) -> Vec<String> {
    let addr = btc.new_address();
    btc.mine(n, &addr)
}

/// Polls until `f` yields a result; panics on timeout.
pub fn wait<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let started = ksana_node::monotonic_now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An in-process fast-lane node (a producer with a funding wallet), connected to the devnet's regtest bitcoind.
pub struct Fast {
    pub node: Option<Node>,
    pub client: HttpClient,
    pub rt: tokio::runtime::Runtime,
    pub _dir: tempfile::TempDir,
    pub producer: User,
    pub funding: Funding,
    /// The genesis config used by the node (the indexer uses the same one).
    pub genesis: GenesisFile,
    /// The mining algorithm used by the node, `None` for the network's (the indexer uses the same one, D-094).
    pub pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
}

impl Fast {
    /// The funding wallet receives `utxos` confirmed UTXOs of 0.002 BTC; the genesis anchor is the current BTC tip.
    pub fn start(btc: &Btc, name: &str, allocations: &[(&User, u128)], utxos: usize) -> Fast {
        // PoW at its minimum; limit the block production rate so that a settlement transaction carries at most
        // MAX_SETTLEMENT_HEADERS headers
        Fast::start_with(btc, name, allocations, utxos, Duration::from_millis(700))
    }

    /// Like [`Fast::start`], with the minimum block interval given by the caller.
    pub fn start_with(
        btc: &Btc,
        name: &str,
        allocations: &[(&User, u128)],
        utxos: usize,
        min_block_interval: Duration,
    ) -> Fast {
        Fast::start_inner(
            btc,
            name,
            allocations,
            utxos,
            min_block_interval,
            Cors::Disabled,
        )
    }

    /// Like [`Fast::start`], with the RPC allowing cross-origin calls from browser pages according to `cors` (D-055).
    pub fn start_with_cors(
        btc: &Btc,
        name: &str,
        allocations: &[(&User, u128)],
        utxos: usize,
        cors: Cors,
    ) -> Fast {
        Fast::start_inner(
            btc,
            name,
            allocations,
            utxos,
            Duration::from_millis(700),
            cors,
        )
    }

    /// Like [`Fast::start`], with the producer configured with a revenue address and a hot-wallet cap (D-060).
    pub fn start_with_revenue(
        btc: &Btc,
        name: &str,
        allocations: &[(&User, u128)],
        utxos: usize,
        revenue: Revenue,
    ) -> Fast {
        Fast::start_full(
            btc,
            name,
            allocations,
            utxos,
            Duration::from_millis(700),
            Cors::Disabled,
            Some(revenue),
            None,
        )
    }

    /// Like [`Fast::start_with`], with the node (and the indexer started later with [`indexer`]) producing and
    /// verifying blocks with the given mining algorithm (the injection point of D-062): test cases that produce
    /// hundreds of empty blocks in a row use the test algorithm `Sha256d`, since once the empty-block streak weight
    /// reaches 120, Argon2id takes about 0.25 seconds per block (D-094).
    pub fn start_with_algorithm(
        btc: &Btc,
        name: &str,
        allocations: &[(&User, u128)],
        utxos: usize,
        min_block_interval: Duration,
        pow_algorithm: Arc<dyn PowAlgorithm>,
    ) -> Fast {
        Fast::start_full(
            btc,
            name,
            allocations,
            utxos,
            min_block_interval,
            Cors::Disabled,
            None,
            Some(pow_algorithm),
        )
    }

    fn start_inner(
        btc: &Btc,
        name: &str,
        allocations: &[(&User, u128)],
        utxos: usize,
        min_block_interval: Duration,
        rpc_cors: Cors,
    ) -> Fast {
        Fast::start_full(
            btc,
            name,
            allocations,
            utxos,
            min_block_interval,
            rpc_cors,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_full(
        btc: &Btc,
        name: &str,
        allocations: &[(&User, u128)],
        utxos: usize,
        min_block_interval: Duration,
        rpc_cors: Cors,
        revenue: Option<Revenue>,
        pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
    ) -> Fast {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &btc_secret(&format!("{name}/funding")));
        let funding = Funding::new(keypair, 2);
        let funding = match revenue {
            Some(revenue) => funding.with_revenue(revenue),
            None => funding,
        };
        let outputs = (0..utxos)
            .map(|_| TxOut {
                value: Amount::from_sat(200_000),
                script_pubkey: funding.script.clone(),
            })
            .collect();
        pay(btc, outputs);
        mine(btc, 1);

        let producer = user(&format!("{name}/producer"));
        let genesis = GenesisFile {
            timestamp: genesis_timestamp(),
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
        let dir = tempfile::tempdir().unwrap();
        let node = Node::start(NodeConfig {
            data_dir: dir.path().join("data"),
            genesis: genesis.clone(),
            params: DEVNET,
            rpc_addr: "127.0.0.1:0".parse().unwrap(),
            rpc_cors,
            producer: Some(producer.account),
            carrier_payee: None,
            funding: Some(funding.clone()),
            policy: Policy::default(),
            btc: Arc::new(RpcClient::new(&url, &rpc_user, &rpc_pass).unwrap()),
            btc_poll: Duration::from_millis(100),
            min_block_interval,
            net: None,
            pow_algorithm: pow_algorithm.clone(),
            pruning: ksana_node::chain::Pruning::default(),
        })
        .unwrap();
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(60))
            .build(format!("http://{}", node.rpc_addr()))
            .unwrap();
        Fast {
            node: Some(node),
            client,
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
            _dir: dir,
            producer,
            funding,
            genesis,
            pow_algorithm,
        }
    }

    pub fn node(&self) -> &Node {
        self.node.as_ref().unwrap()
    }

    pub fn submit(&self, op: &VmOp) -> Result<[u8; 32], String> {
        self.rt
            .block_on(self.client.submit_op(hex::encode(op.encode().unwrap())))
            .map(|r| r.op_id)
            .map_err(|e| e.to_string())
    }

    pub fn status(&self, op_id: &[u8; 32]) -> OpStatusJson {
        self.rt
            .block_on(self.client.get_op_status(hex::encode(op_id)))
            .unwrap()
    }

    pub fn block(&self, height: u64) -> BlockJson {
        self.rt
            .block_on(self.client.get_block(height.to_string()))
            .unwrap()
    }

    pub fn settlement(&self, height: u64) -> SettlementJson {
        self.rt
            .block_on(self.client.get_settlement(height.to_string()))
            .unwrap()
    }

    pub fn ksa(&self, u: &User) -> u128 {
        let account = self
            .rt
            .block_on(self.client.get_account(hex::encode(u.account)))
            .unwrap();
        account
            .balances
            .iter()
            .find(|b| b.asset == KSA_ASSET_ID)
            .map_or(0, |b| b.amount)
    }

    pub fn locked(&self, u: &User, block: &[u8; 32]) -> Option<u128> {
        let account = self
            .rt
            .block_on(self.client.get_account(hex::encode(u.account)))
            .unwrap();
        account
            .locked_fees
            .iter()
            .find(|l| l.block_hash == *block)
            .map(|l| l.amount)
    }

    pub fn tip_height(&self) -> u64 {
        self.node().chain().tip().unwrap().header.height
    }

    /// Waits for the op to be included and returns its status.
    /// Waits for the op to be included (the node does not know a native transaction before it is included; the query
    /// returns "unknown op").
    pub fn wait_included(&self, op_id: &[u8; 32]) -> OpStatusJson {
        wait("the op to be included", Duration::from_secs(60), || {
            let s = self
                .rt
                .block_on(self.client.get_op_status(hex::encode(op_id)))
                .ok()?;
            (s.status != OpState::Pending).then_some(s)
        })
    }

    /// Waits for the main chain to reach `height`.
    pub fn wait_height(&self, height: u64) {
        wait("the fast-lane height", Duration::from_secs(60), || {
            (self.tip_height() >= height).then_some(())
        });
    }

    /// Waits for a block's settlement transaction to enter `state`.
    pub fn wait_settlement(&self, height: u64, state: SettlementState) -> SettlementJson {
        wait("the settlement state", Duration::from_secs(60), || {
            let s = self.settlement(height);
            (s.status == state).then_some(s)
        })
    }
}

impl Drop for Fast {
    fn drop(&mut self) {
        if let Some(node) = self.node.take() {
            node.stop().unwrap();
        }
    }
}

/// A BTC client that records the transactions broadcast through it (one per node).
pub struct Recorder {
    pub inner: RpcClient,
    pub broadcasts: Mutex<Vec<Transaction>>,
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

    /// The op's status; `None` when the node does not know the op.
    pub fn status(&self, op_id: &[u8; 32]) -> Option<OpStatusJson> {
        self.rt
            .block_on(self.client.get_op_status(hex::encode(op_id)))
            .ok()
    }

    pub fn settlement(&self, height: u64) -> SettlementJson {
        self.rt
            .block_on(self.client.get_settlement(height.to_string()))
            .unwrap()
    }

    pub fn wait_settlement(&self, height: u64, state: SettlementState) -> SettlementJson {
        wait("the settlement state", Duration::from_secs(60), || {
            let s = self.settlement(height);
            (s.status == state).then_some(s)
        })
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

/// The producers of the main-chain blocks at heights 1..=tip−`skip`.
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

/// Network tuning for tests: shorter intervals for the periodic Status query and redialing.
pub fn tuning() -> Tuning {
    Tuning {
        status_interval: Duration::from_millis(500),
        redial_interval: Duration::from_millis(300),
        request_timeout: Duration::from_secs(10),
        ..Tuning::default()
    }
}

pub struct Net {
    pub nodes: Vec<TestNode>,
}

impl Net {
    /// Starts 3 nodes (each funding wallet has 4 confirmed UTXOs; the genesis anchor is the current BTC tip), each
    /// using the previously started nodes as bootstrap peers; all of them pause block production after starting.
    pub fn start(btc: &Btc, name: &str, allocations: &[(&User, u128)], interval: Duration) -> Net {
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
            timestamp: genesis_timestamp(),
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

    pub fn wait_connected(&self, group: &[usize]) {
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

    pub fn partition(&self, left: &[usize], right: &[usize]) {
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

    pub fn heal(&self, left: &[usize], right: &[usize]) {
        for &l in left {
            for &r in right {
                self.nodes[l].net().unblock_peer(self.nodes[r].peer_id);
                self.nodes[r].net().unblock_peer(self.nodes[l].peer_id);
            }
        }
        self.wait_connected(&[0, 1, 2]);
    }

    pub fn converge(&self) -> Tip {
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

/// An independent indexer connected only to bitcoind (a fresh data directory, the same genesis config and mining
/// algorithm as the node, D-048, D-094).
pub fn indexer(fast: &Fast) -> (Indexer, tempfile::TempDir) {
    let (url, rpc_user, rpc_pass) = btc_env();
    let dir = tempfile::tempdir().unwrap();
    let indexer = Indexer::open(IndexerConfig {
        data_dir: dir.path().to_path_buf(),
        genesis: fast.genesis.clone(),
        params: DEVNET,
        btc: Arc::new(RpcClient::new(&url, &rpc_user, &rpc_pass).unwrap()),
        pow_algorithm: fast.pow_algorithm.clone(),
        pruning: ksana_btc::prune::Pruning::default(),
    })
    .unwrap();
    (indexer, dir)
}

/// Mines BTC blocks while waiting, until the block at height `height` on the node's main chain (whether or not it
/// carries a settlement transaction) enters the settled prefix (D-038, D-063).
pub fn mine_until_prefix(btc: &Btc, fast: &Fast, height: u64) {
    wait(
        "entry into the settled prefix",
        Duration::from_secs(120),
        || {
            mine(btc, 1);
            std::thread::sleep(Duration::from_millis(300));
            let chain = fast.node().chain();
            let snapshot = chain.btc().snapshot().ok()?;
            chain.prefix_height(&snapshot, height).ok()?.map(|_| ())
        },
    );
}

/// Before the indexer executes up to `height`, lets the node's settled prefix pass it by `SETTLEMENT_DEPTH` blocks:
/// the indexer executes a main-chain block only once it has that many confirmations on its main chain (D-063).
pub fn settle_for_indexer(btc: &Btc, fast: &Fast, height: u64) {
    mine_until_prefix(btc, fast, height + DEVNET.settlement_depth);
}

/// Syncs one round and returns the status; the test fails if the indexer halted (found an invalid block).
pub fn index_once(indexer: &Indexer) -> IndexerStatus {
    indexer.sync_once().unwrap();
    let status = indexer.status();
    assert!(
        status.halted.is_none(),
        "indexer halted: {:?}",
        status.halted
    );
    status
}

/// Syncs the indexer to at least `height`, then checks that the block hash and `state_root` at every height it
/// executed equal the node's (`node_block(h)` gets the node's block at height h). Returns the indexer's status.
pub fn index_and_compare(
    indexer: &Indexer,
    height: u64,
    node_block: impl Fn(u64) -> BlockJson,
) -> IndexerStatus {
    let status = wait("the indexer to catch up", Duration::from_secs(120), || {
        let status = index_once(indexer);
        (status.height >= height).then_some(status)
    });
    for h in 1..=status.height {
        let (hash, root) = indexer.block_at(h).unwrap();
        let block = node_block(h);
        assert_eq!(hash, block.hash, "block hash at height {h}");
        assert_eq!(root, block.header.state_root, "state_root at height {h}");
    }
    let tip = node_block(status.height);
    assert_eq!(
        (status.hash, status.state_root),
        (tip.hash, tip.header.state_root)
    );
    status
}

// ---------------------------------------------------------------------------
// TypeScript SDK end-to-end (m6, m13)

/// Runs the built SDK script `sdk/ts/dist/test/e2e/<script>` (the input is one JSON argument) and returns the JSON
/// it prints on the last line of stdout.
pub fn run_sdk(script: &str, input: &Value) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/ts/dist/test/e2e")
        .join(script);
    assert!(
        path.exists(),
        "missing {}: build the TypeScript SDK first (cd sdk/ts && npm ci && npm run build; tests/integration/run.sh builds it automatically)",
        path.display()
    );
    let node = std::env::var("KSANA_NODEJS").unwrap_or_else(|_| "node".to_string());
    let out = Command::new(&node)
        .arg(&path)
        .arg(input.to_string())
        .output()
        .unwrap_or_else(|e| panic!("running {node} (Node.js ≥ 20 required): {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{script} failed:\n{}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    let last = stdout.lines().last().unwrap_or_default();
    serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("output of {script} is not JSON ({e}): {stdout}"))
}

pub fn op_id_of(out: &Value) -> [u8; 32] {
    hex::decode(out["op_id"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap()
}

/// The op's status; `None` while the node does not know it yet (before a native transaction is included).
pub fn op_status(fast: &Fast, op_id: &[u8; 32]) -> Option<OpStatusJson> {
    fast.rt
        .block_on(fast.client.get_op_status(hex::encode(op_id)))
        .ok()
}

/// A wallet UTXO, with the txid in the display byte order of bitcoind and wallets (the same as Unisat's
/// `getBitcoinUtxos`).
pub fn utxo_json(point: &BtcOutPoint, value: u64) -> Value {
    json!({
        "txid": point.txid.to_string(),
        "vout": point.vout,
        "value": value.to_string(),
    })
}

/// The locking script `OP_1 <account>` of the wallet (account) address: the account is the BIP86 output public key
/// (D-092), derived the same way as the funding script.
pub fn wallet_script(u: &User) -> ScriptBuf {
    let script = ksana_crypto::account_script(&u.account).unwrap();
    assert_eq!(
        script,
        ksana_btc::settlement::producer_script(&u.kp.x_only_public_key().0)
    );
    script
}

/// The locking script of the P2WPKH address of the same secret key: not an account (since D-092 accounts only have
/// Taproot addresses), used only as a payment script of an arbitrary type.
pub fn p2wpkh_script(u: &User) -> ScriptBuf {
    let secp = Secp256k1::new();
    let secret = SecretKey::from_slice(&u.kp.secret_bytes()).unwrap();
    let public = bitcoin::CompressedPublicKey(secret.public_key(&secp));
    ScriptBuf::new_p2wpkh(&public.wpubkey_hash())
}

/// Pays `value` sats to each script (in one transaction) and mines 1 block to confirm; returns the outputs.
pub fn fund_wallets(btc: &Btc, scripts: Vec<ScriptBuf>, value: u64) -> Vec<BtcOutPoint> {
    let outputs = scripts
        .into_iter()
        .map(|script_pubkey| TxOut {
            value: Amount::from_sat(value),
            script_pubkey,
        })
        .collect();
    let (_, points) = pay(btc, outputs);
    mine(btc, 1);
    points
}

/// Genesis timestamp of the test chain: 30 days in the past. An empty block's weight is 12-120 depending on the streak
/// (D-084, D-090), so every block advances the ASERT schedule by 12-120 block intervals; a test chain producing blocks
/// at sub-second intervals quickly runs far ahead of schedule and the difficulty doubles about every 12 empty blocks.
/// With the genesis timestamp moved back, the chain always lags behind the schedule (enough for over 4 000 empty
/// blocks of weight 120), and the difficulty stays at the PoW limit.
pub fn genesis_timestamp() -> u64 {
    ksana_node::unix_now() - 30 * 24 * 3600
}
