//! Single-node test fixture: an in-process node (block production, RPC, BTC thread) connected to the in-memory mock BTC
//! chain `MockBtc`. Shared by the M3 and M5 node tests.

use std::sync::Arc;
use std::time::Duration;

use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::{
    absolute, transaction, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use ksana_btc::client::BtcClient;
use ksana_btc::data::data_script;
use ksana_btc::mock::MockBtc;
use ksana_btc::native::outpoint_of;
use ksana_consensus::PowAlgorithm;
use ksana_crypto::{sign_native_vmop, sign_vmop};
use ksana_mempool::Policy;
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::node::{Node, NodeConfig};
use ksana_node::producer::Funding;
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, BtcAnchor, FeeMode, Payload, SigScheme, VmOp, DEVNET, KSA_ASSET_ID, VMOP_VERSION,
};
use ksana_rpc::{KsanaApiClient, OpState, OpStatusJson, SettlementJson, SettlementState};

use super::{genesis_timestamp, user, User};

/// A path-B KSA transfer op. For path C use [`signed_native`].
pub fn signed(u: &User, nonce: u64, fee: FeeMode, to: [u8; 32], amount: u128) -> VmOp {
    let mut op = unsigned(u, nonce, fee, to, amount);
    op.sig = sign_vmop(&op, &u.kp, &[2; 32]).unwrap();
    op
}

/// A path-C KSA transfer op: the signature binds the first input `first_input` of the carrying transaction (D-086);
/// used together with [`native_tx`].
pub fn signed_native(
    u: &User,
    nonce: u64,
    first_input: &bitcoin::OutPoint,
    to: [u8; 32],
    amount: u128,
) -> VmOp {
    let mut op = unsigned(u, nonce, FeeMode::BtcNative, to, amount);
    op.sig = sign_native_vmop(&op, &outpoint_of(first_input), &u.kp, &[2; 32]).unwrap();
    op
}

fn unsigned(u: &User, nonce: u64, fee: FeeMode, to: [u8; 32], amount: u128) -> VmOp {
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

/// A path-C native transaction: spends new funds on the mock chain (a confirmed coinbase output), and the KSN1 data
/// output carries the op given by `op_for(first input)` (whose signature binds it, D-086). Returns the transaction and
/// the op.
pub fn native_tx(
    mock: &MockBtc,
    op_for: impl FnOnce(&bitcoin::OutPoint) -> VmOp,
) -> (Transaction, VmOp) {
    let input = mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0];
    let op = op_for(&input);
    let payload = Payload::NativeOp { op: op.clone() }
        .encode(&DEVNET)
        .unwrap();
    let tx = Transaction {
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
            script_pubkey: data_script(&DEVNET.magic, &payload).unwrap(),
        }],
    };
    (tx, op)
}

pub fn btc_secret(seed: &str) -> SecretKey {
    SecretKey::from_slice(&sha256(format!("ksana/node-test/m3/{seed}").as_bytes())).unwrap()
}

pub struct MockNode {
    pub node: Option<Node>,
    pub mock: Arc<MockBtc>,
    pub client: HttpClient,
    pub rt: tokio::runtime::Runtime,
    pub producer: User,
    pub funding: Funding,
    pub _dir: tempfile::TempDir,
}

impl MockNode {
    pub fn start(allocations: &[(&User, u128)]) -> MockNode {
        Self::start_with(allocations, Duration::from_millis(150))
    }

    /// The minimum interval between two blocks is `interval`.
    pub fn start_with(allocations: &[(&User, u128)], interval: Duration) -> MockNode {
        Self::start_wrapped(allocations, interval, |mock| mock)
    }

    /// Same as [`MockNode::start_with`], but the node reaches the mock chain through `wrap(mock)` (test wrappers:
    /// delayed returns, recording transactions, etc.).
    pub fn start_wrapped(
        allocations: &[(&User, u128)],
        interval: Duration,
        wrap: impl FnOnce(Arc<MockBtc>) -> Arc<dyn BtcClient>,
    ) -> MockNode {
        Self::start_custom(allocations, interval, wrap, |funding| funding)
    }

    /// Same as [`MockNode::start_wrapped`], but the funding wallet is adjusted by `configure` (e.g. setting the revenue
    /// address, D-060).
    pub fn start_custom(
        allocations: &[(&User, u128)],
        interval: Duration,
        wrap: impl FnOnce(Arc<MockBtc>) -> Arc<dyn BtcClient>,
        configure: impl FnOnce(Funding) -> Funding,
    ) -> MockNode {
        Self::start_with_params(DEVNET, allocations, interval, wrap, configure)
    }

    /// Same as [`MockNode::start_custom`], but running with `params` (the default is DEVNET).
    pub fn start_with_params(
        params: ksana_primitives::Params,
        allocations: &[(&User, u128)],
        interval: Duration,
        wrap: impl FnOnce(Arc<MockBtc>) -> Arc<dyn BtcClient>,
        configure: impl FnOnce(Funding) -> Funding,
    ) -> MockNode {
        Self::start_full(params, allocations, None, interval, wrap, configure)
    }

    /// Same as [`MockNode::start_with_params`], but the genesis config has a block share account (D-089).
    pub fn start_full(
        params: ksana_primitives::Params,
        allocations: &[(&User, u128)],
        block_share_account: Option<[u8; 32]>,
        interval: Duration,
        wrap: impl FnOnce(Arc<MockBtc>) -> Arc<dyn BtcClient>,
        configure: impl FnOnce(Funding) -> Funding,
    ) -> MockNode {
        Self::start_inner(
            params,
            allocations,
            block_share_account,
            interval,
            wrap,
            configure,
            None,
        )
    }

    /// Same as [`MockNode::start_wrapped`], but the node uses the given mining algorithm (D-062) instead of the
    /// network's Argon2id (D-094): tests that produce hundreds of empty blocks use the fast test algorithm `Sha256d`
    /// (once the empty-block streak weight reaches 120, Argon2id takes about 0.25 seconds per block).
    pub fn start_with_algorithm(
        allocations: &[(&User, u128)],
        interval: Duration,
        wrap: impl FnOnce(Arc<MockBtc>) -> Arc<dyn BtcClient>,
        pow_algorithm: Arc<dyn PowAlgorithm>,
    ) -> MockNode {
        Self::start_inner(
            DEVNET,
            allocations,
            None,
            interval,
            wrap,
            |funding| funding,
            Some(pow_algorithm),
        )
    }

    /// Same as [`MockNode::start_with_algorithm`], but running with `params`.
    pub fn start_with_params_and_algorithm(
        params: ksana_primitives::Params,
        allocations: &[(&User, u128)],
        interval: Duration,
        pow_algorithm: Arc<dyn PowAlgorithm>,
    ) -> MockNode {
        Self::start_inner(
            params,
            allocations,
            None,
            interval,
            |mock| mock,
            |funding| funding,
            Some(pow_algorithm),
        )
    }

    fn start_inner(
        params: ksana_primitives::Params,
        allocations: &[(&User, u128)],
        block_share_account: Option<[u8; 32]>,
        interval: Duration,
        wrap: impl FnOnce(Arc<MockBtc>) -> Arc<dyn BtcClient>,
        configure: impl FnOnce(Funding) -> Funding,
        pow_algorithm: Option<Arc<dyn PowAlgorithm>>,
    ) -> MockNode {
        let mock = Arc::new(MockBtc::new());
        let secp = Secp256k1::new();
        let funding = configure(Funding::new(
            Keypair::from_secret_key(&secp, &btc_secret("funding")),
            2,
        ));
        mock.fund(&funding.script, &[500_000; 8]);
        let (height, hash) = mock.tip().unwrap();
        let producer = user("m3/producer");
        let dir = tempfile::tempdir().unwrap();
        let node = Node::start(NodeConfig {
            data_dir: dir.path().join("data"),
            genesis: GenesisFile {
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
            },
            params,
            rpc_addr: "127.0.0.1:0".parse().unwrap(),

            rpc_cors: Default::default(),
            producer: Some(producer.account),
            carrier_payee: None,
            funding: Some(funding.clone()),
            policy: Policy::default(),
            btc: wrap(mock.clone()),
            btc_poll: Duration::from_millis(50),
            min_block_interval: interval,
            net: None,
            pow_algorithm,
            pruning: ksana_node::chain::Pruning::default(),
        })
        .unwrap();
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(30))
            .build(format!("http://{}", node.rpc_addr()))
            .unwrap();
        MockNode {
            node: Some(node),
            mock,
            client,
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
            producer,
            funding,
            _dir: dir,
        }
    }

    pub fn node(&self) -> &Node {
        self.node.as_ref().unwrap()
    }

    pub fn tip_height(&self) -> u64 {
        self.node().chain().tip().unwrap().header.height
    }

    pub fn submit(&self, op: &VmOp) -> [u8; 32] {
        self.rt
            .block_on(self.client.submit_op(hex::encode(op.encode().unwrap())))
            .unwrap()
            .op_id
    }

    pub fn settlement(&self, height: u64) -> SettlementJson {
        self.rt
            .block_on(self.client.get_settlement(height.to_string()))
            .unwrap()
    }

    pub fn account(&self, u: &User) -> ksana_rpc::AccountJson {
        self.rt
            .block_on(self.client.get_account(hex::encode(u.account)))
            .unwrap()
    }

    /// Submit an op and return the RPC error (for when it should be rejected).
    pub fn submit_err(&self, op: &VmOp) -> jsonrpsee::core::ClientError {
        self.rt
            .block_on(self.client.submit_op(hex::encode(op.encode().unwrap())))
            .unwrap_err()
    }

    pub fn ksa(&self, u: &User) -> u128 {
        let a = self.account(u);
        a.balances
            .iter()
            .find(|b| b.asset == KSA_ASSET_ID)
            .map_or(0, |b| b.amount)
    }

    pub fn block(&self, height: u64) -> ksana_rpc::BlockJson {
        self.rt
            .block_on(self.client.get_block(height.to_string()))
            .unwrap()
    }

    /// Wait for the op to be included (the node does not know a native transaction before it is included; the query
    /// returns "unknown op").
    pub fn wait_included(&self, op_id: &[u8; 32]) -> OpStatusJson {
        wait("the op to be included", || {
            let s = self
                .rt
                .block_on(self.client.get_op_status(hex::encode(op_id)))
                .ok()?;
            (s.status != OpState::Pending).then_some(s)
        })
    }

    /// The main-chain tip and protocol version (`ksana_getChainInfo`, D-076).
    pub fn chain_info(&self) -> ksana_rpc::ChainInfoJson {
        self.rt.block_on(self.client.get_chain_info()).unwrap()
    }

    pub fn wait_settlement(&self, height: u64, state: SettlementState) -> SettlementJson {
        wait("the settlement state", || {
            let s = self.settlement(height);
            (s.status == state).then_some(s)
        })
    }
}

impl Drop for MockNode {
    fn drop(&mut self) {
        if let Some(node) = self.node.take() {
            node.stop().unwrap();
        }
    }
}

pub fn wait<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let started = ksana_node::monotonic_now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
