//! M3 acceptance tests (SPEC §14 M3, `tests/integration/run.sh`): the devnet's regtest bitcoind + an in-process
//! `ksana-node` (block production, BTC anchoring, forced inclusion, settlement transaction funding / signing /
//! delayed broadcast, unlock entries).
//!
//! 1. Path B: the op takes effect within a fast-lane block, the settlement transaction (only the producer's inputs;
//!    outputs are change, data and P2A) confirms on regtest, and the fee is paid by the producer's funding (formerly
//!    path A: the remainder of the contribution went into the producer's change; path A has been removed, D-078).
//! 2. One settlement transaction covers the ops of 20 different users and lands on BTC (formerly merging the path-A
//!    input/output pairs of 20 users).
//! 3. (Removed with path A, D-078) Formerly "after removing any one input/output pair, the other users' signatures
//!    remain valid": the settlement transaction no longer has user input/output pairs.
//! 4. Path C: after the native transaction confirms, it is force-included at the start of the first eligible fast-lane
//!    block.
//! 5. No spending relationship between settlement transactions: several settlements in the mempool at the same time
//!    have no parent-child relationship with each other and confirm in the same BTC block.
//! 6. One CPFP through the P2A output: after the settlement transaction is "stuck" (its mining priority lowered to
//!    simulate a fee spike), anyone spends the P2A output to speed up its landing.
//! 7. Unlock: cannot unlock while the settlement is unconfirmed, or confirmed but an earlier block has not landed yet
//!    (not in the settled prefix); once all are confirmed the producer writes an unlock entry and the locked token
//!    fees become available balance; the same block cannot be unlocked twice.
//!
//! Each test uses its own funding secret key, users and data directory; only the test itself mines BTC blocks. Run in
//! order with `--test-threads=1`.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::{
    absolute, transaction, Amount, BlockHash, OutPoint as BtcOutPoint, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Txid, Witness,
};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use ksana_btc::client::RpcClient;
use ksana_btc::payload::{attached_payload, envelope_payload};
use ksana_btc::settlement::{self as btc_settlement, SettlementPair};
use ksana_crypto::{account_of, keypair_from_secret, sign_native_vmop, sign_vmop};
use ksana_execution::{BlockError, BtcView};
use ksana_integration_tests::Btc;
use ksana_mempool::Policy;
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::node::{Node, NodeConfig};
use ksana_node::producer::Funding;
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, BtcAnchor, Entry, FeeMode, OutPoint, Payload, SigScheme, VmOp, DEVNET, KSA_ASSET_ID,
    VMOP_VERSION,
};
use ksana_rpc::{
    BlockJson, KsanaApiClient, OpState, OpStatusJson, SettlementJson, SettlementState,
};
use serde_json::json;

/// VmOp account of a user or the producer.
struct User {
    kp: ksana_crypto::Keypair,
    account: [u8; 32],
}

fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(format!("ksana/it/m3/{seed}").as_bytes())).unwrap();
    User {
        account: account_of(&kp),
        kp,
    }
}

fn signed(u: &User, nonce: u64, fee: FeeMode, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee,
        max_gas: 100_000,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[5; 32]).unwrap();
    op
}

/// A signed path-C op: the signature is bound to the carrying transaction's first input `first_input` (D-086).
fn signed_native(u: &User, nonce: u64, first_input: &OutPoint, action: Action) -> VmOp {
    let mut op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::BtcNative,
        max_gas: 100_000,
        action,
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_native_vmop(&op, first_input, &u.kp, &[5; 32]).unwrap();
    op
}

fn transfer(to: [u8; 32], amount: u128) -> Action {
    Action::AssetTransfer {
        asset: KSA_ASSET_ID,
        to,
        amount,
    }
}

/// A BTC secret key for tests (deterministic).
fn btc_secret(seed: &str) -> SecretKey {
    SecretKey::from_slice(&sha256(format!("ksana/it/m3/btc/{seed}").as_bytes())).unwrap()
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

/// A transaction funded by the wallet that pays `outputs`, broadcast; returns the transaction and each output's
/// outpoint.
fn pay(btc: &Btc, outputs: Vec<TxOut>) -> (Transaction, Vec<BtcOutPoint>) {
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

fn mine(btc: &Btc, n: u64) -> Vec<String> {
    let addr = btc.new_address();
    btc.mine(n, &addr)
}

fn confirmations(btc: &Btc, txid: &Txid) -> u64 {
    btc.try_call("getrawtransaction", &[json!(txid.to_string()), json!(true)])
        .ok()
        .and_then(|v| v["confirmations"].as_u64())
        .unwrap_or(0)
}

fn in_mempool(btc: &Btc, txid: &Txid) -> bool {
    btc.try_call("getmempoolentry", &[json!(txid.to_string())])
        .is_ok()
}

/// An in-process fast-lane node (a producer with a funding wallet), connected to the devnet's regtest bitcoind.
struct Fast {
    node: Option<Node>,
    client: HttpClient,
    rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
    producer: User,
    funding: Funding,
}

impl Fast {
    /// The funding wallet receives `utxos` confirmed UTXOs of 0.002 BTC; the genesis anchor is the current BTC tip.
    fn start(btc: &Btc, name: &str, allocations: &[(&User, u128)], utxos: usize) -> Fast {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &btc_secret(&format!("{name}/funding")));
        // The M3 scenarios are written for per-block settlement (every block with ops carries its own settlement
        // transaction); for settlement batching see m12_regtest.rs (D-077)
        let funding = Funding::new(keypair, 2).with_batching(false);
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
        let dir = tempfile::tempdir().unwrap();
        let node = Node::start(NodeConfig {
            data_dir: dir.path().join("data"),
            genesis,
            params: DEVNET,
            rpc_addr: "127.0.0.1:0".parse().unwrap(),

            rpc_cors: Default::default(),
            producer: Some(producer.account),
            carrier_payee: None,
            funding: Some(funding.clone()),
            policy: Policy::default(),
            btc: Arc::new(RpcClient::new(&url, &rpc_user, &rpc_pass).unwrap()),
            btc_poll: Duration::from_millis(100),
            // PoW at its minimum; limit the block production rate so that a settlement transaction carries at most
            // MAX_SETTLEMENT_HEADERS headers
            min_block_interval: Duration::from_millis(700),
            net: None,
            pow_algorithm: None,
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
        }
    }

    fn node(&self) -> &Node {
        self.node.as_ref().unwrap()
    }

    fn submit(&self, op: &VmOp) -> Result<[u8; 32], String> {
        self.rt
            .block_on(self.client.submit_op(hex::encode(op.encode().unwrap())))
            .map(|r| r.op_id)
            .map_err(|e| e.to_string())
    }

    fn status(&self, op_id: &[u8; 32]) -> OpStatusJson {
        self.rt
            .block_on(self.client.get_op_status(hex::encode(op_id)))
            .unwrap()
    }

    fn block(&self, height: u64) -> BlockJson {
        self.rt
            .block_on(self.client.get_block(height.to_string()))
            .unwrap()
    }

    fn settlement(&self, height: u64) -> SettlementJson {
        self.rt
            .block_on(self.client.get_settlement(height.to_string()))
            .unwrap()
    }

    fn ksa(&self, u: &User) -> u128 {
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

    fn locked(&self, u: &User, block: &[u8; 32]) -> Option<u128> {
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

    fn tip_height(&self) -> u64 {
        self.node().chain().tip().unwrap().header.height
    }

    /// Waits for the op to be included and returns its status.
    /// Waits for the op to be included (the node does not know a native transaction before it is included; the query
    /// returns "unknown op").
    fn wait_included(&self, op_id: &[u8; 32]) -> OpStatusJson {
        wait("the op to be included", Duration::from_secs(60), || {
            let s = self
                .rt
                .block_on(self.client.get_op_status(hex::encode(op_id)))
                .ok()?;
            (s.status != OpState::Pending).then_some(s)
        })
    }

    /// Waits for the main chain to reach `height`.
    fn wait_height(&self, height: u64) {
        wait("the fast-lane height", Duration::from_secs(60), || {
            (self.tip_height() >= height).then_some(())
        });
    }

    /// Waits for a block's settlement transaction to enter `state`.
    fn wait_settlement(&self, height: u64, state: SettlementState) -> SettlementJson {
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

/// Polls until `f` yields a result; panics on timeout.
fn wait<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let started = ksana_node::monotonic_now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(started.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The settlement transactions (commit and reveal, D-081).
fn decode_pair(s: &SettlementJson) -> SettlementPair {
    SettlementPair::decode(
        s.txs
            .as_ref()
            .expect("the block carries settlement transactions"),
    )
    .unwrap()
}

fn txid_of(s: &SettlementJson) -> Txid {
    Txid::from_byte_array(s.txid.expect("the block carries settlement transactions"))
}

/// A transaction in bitcoind's mempool other than `except` whose envelope carries `payload` (a reveal transaction).
fn payload_in_mempool(btc: &Btc, payload: &[u8], except: &Txid) -> Option<Transaction> {
    let ids = btc.call("getrawmempool", &[]);
    ids.as_array()
        .unwrap()
        .iter()
        .filter(|id| id.as_str() != Some(except.to_string().as_str()))
        .filter_map(|id| {
            let hex = btc
                .try_call("getrawtransaction", std::slice::from_ref(id))
                .ok()?;
            deserialize_hex::<Transaction>(hex.as_str()?).ok()
        })
        .find(|tx| envelope_payload(&DEVNET, tx).is_some_and(|carried| carried == payload))
}

/// The sum of the previous outputs' amounts (read from bitcoind).
fn input_value(btc: &Btc, tx: &Transaction) -> u64 {
    tx.input
        .iter()
        .map(|input| {
            let prev: Transaction = deserialize_hex(
                btc.call(
                    "getrawtransaction",
                    &[json!(input.previous_output.txid.to_string())],
                )
                .as_str()
                .unwrap(),
            )
            .unwrap();
            prev.output[input.previous_output.vout as usize]
                .value
                .to_sat()
        })
        .sum()
}

/// Acceptance 1 (revised by D-078, D-081): a path-B op takes effect within a fast-lane block; the settlement consists
/// of a commit and a reveal transaction: the commit transaction has only the producer's inputs, with the change and
/// the P2TR output committing to the envelope as outputs; the reveal transaction spends it, carries the payload in
/// its witness, and has P2A and an empty OP_RETURN as outputs; broadcast after a delay of `SETTLEMENT_DEPTH` blocks,
/// accepted and confirmed by bitcoind, with both fees paid by the producer's funding; BTC finality follows.
#[test]
fn path_b_op_takes_effect_and_the_producer_pays_the_settlement() {
    let btc = Btc::from_env().funded_wallet("m3");
    let alice = user("path-b/alice");
    let bob = user("path-b/bob");
    let fast = Fast::start(&btc, "path-b", &[(&alice, 1_000)], 4);
    fast.wait_height(1);

    let op = signed(
        &alice,
        0,
        FeeMode::Token { amount: 5 },
        transfer(bob.account, 250),
    );
    let op_id = fast.submit(&op).unwrap();
    let status = fast.wait_included(&op_id);
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    assert_eq!(
        fast.ksa(&bob),
        250,
        "the op takes effect within the fast-lane block"
    );
    assert_eq!(
        fast.ksa(&alice),
        1_000 - 250 - 5,
        "the sponsored fee is deducted from the account"
    );
    let height = status.block.unwrap().height;
    let block = fast.block(height);
    assert!(
        matches!(block.entries[status.index.unwrap() as usize].clone(), Entry::Sponsored { op: o } if o == op)
    );

    // Settlement transactions: all inputs of the commit transaction come from the producer's funding address,
    // broadcast after a delay of SETTLEMENT_DEPTH blocks
    let s = fast.settlement(height);
    assert_eq!(s.status, SettlementState::Pending);
    let pair = decode_pair(&s);
    let (commit, reveal) = (&pair.commit, &pair.reveal);
    assert_eq!(commit.output.len(), 2, "change and commitment");
    assert_eq!(reveal.output.len(), 2, "P2A and empty OP_RETURN");
    assert_eq!(
        s.txid,
        Some(pair.txid()),
        "the settlement's txid is the reveal transaction"
    );
    assert_eq!(s.commit_txid, Some(pair.commit_txid()));
    for input in &commit.input {
        let prev: Transaction = deserialize_hex(
            btc.call(
                "getrawtransaction",
                &[json!(input.previous_output.txid.to_string())],
            )
            .as_str()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            prev.output[input.previous_output.vout as usize].script_pubkey, fast.funding.script,
            "all inputs come from the producer's funding address"
        );
    }
    let broadcast = fast.wait_settlement(height, SettlementState::Broadcast);
    assert!(fast.tip_height() >= height + DEVNET.settlement_depth);
    assert!(in_mempool(&btc, &txid_of(&broadcast)));
    assert!(in_mempool(&btc, &commit.compute_txid()));

    mine(&btc, 1);
    let confirmed = fast.wait_settlement(height, SettlementState::Confirmed);
    let c = confirmed.confirmed.clone().unwrap();
    assert_eq!(Some(c.txid), s.txid);
    assert_eq!(confirmations(&btc, &txid_of(&s)), 1);
    assert_eq!(confirmations(&btc, &commit.compute_txid()), 1);

    // Commit transaction: the change returns to the funding address, fee = producer inputs − change − committed
    // amount; reveal transaction: fee = committed amount − P2A
    let producer_in = input_value(&btc, commit);
    let change = commit.output[0].value.to_sat();
    let committed = commit.output[1].value.to_sat();
    let commit_fee = producer_in - change - committed;
    let reveal_fee = committed - btc_settlement::P2A_AMOUNT_SAT;
    assert_eq!(commit.output[0].script_pubkey, fast.funding.script);
    assert!(
        commit.output[1].script_pubkey.is_p2tr(),
        "commitment output"
    );
    assert_eq!(
        reveal.output[0].script_pubkey,
        btc_settlement::p2a_script(),
        "P2A output"
    );
    assert_eq!(
        reveal.output[0].value.to_sat(),
        btc_settlement::P2A_AMOUNT_SAT
    );
    assert_eq!(
        reveal.output[1].script_pubkey,
        btc_settlement::empty_op_return()
    );
    let min = DEVNET.min_settlement_feerate_sat_vb;
    assert!(commit_fee >= commit.vsize() as u64 * min);
    assert!(reveal_fee >= reveal.vsize() as u64 * min);
    // The payload is in the reveal transaction's witness; the last header is this block
    let payload = ksana_btc::payload::attached_payload(&DEVNET, &pair).unwrap();
    let Payload::Settlement { headers, .. } = Payload::decode(&payload, &DEVNET).unwrap() else {
        panic!("not a settlement payload");
    };
    assert_eq!(headers.last(), Some(&block.header));
    eprintln!(
        "settlement transactions: commit {} vB, reveal {} vB ({} payload bytes in the witness), fees {commit_fee} + {reveal_fee} sats",
        commit.vsize(),
        reveal.vsize(),
        payload.len()
    );

    // Determined final once the data has BTC_FINALITY_CONFS confirmations (D-082), and BTC final once the BTC block of
    // that determination has as many (D-114)
    mine(&btc, 2 * (u64::from(DEVNET.btc_finality_confs) - 1));
    wait("BTC finality", Duration::from_secs(30), || {
        (fast.status(&op_id).status == OpState::BtcFinal).then_some(())
    });
    let s = fast.settlement(height);
    assert!(s.settled && s.btc_final);
}

/// Acceptance 2 (revised by D-078, D-081): the path-B ops of 20 different users enter the same block and land on BTC
/// through one settlement (a commit and a reveal transaction) (the settlement transactions' structure does not depend
/// on the number of users: the inputs are only the producer's funding, the outputs are fixed).
#[test]
fn twenty_users_share_one_settlement() {
    let btc = Btc::from_env().funded_wallet("m3");
    let users: Vec<User> = (0..20).map(|i| user(&format!("twenty/{i}"))).collect();
    let sink = user("twenty/sink");
    let allocations: Vec<(&User, u128)> = users.iter().map(|u| (u, 100)).collect();
    let fast = Fast::start(&btc, "twenty", &allocations, 4);
    fast.wait_height(1);

    // Pause block production and resume once all 20 ops are in the entry pool: they enter the same block
    fast.node().set_paused(true);
    std::thread::sleep(Duration::from_millis(300));
    let ids: Vec<[u8; 32]> = users
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let fee = 1 + i as u128 % 3;
            let op = signed(
                u,
                0,
                FeeMode::Token { amount: fee },
                transfer(sink.account, 1),
            );
            fast.submit(&op).unwrap()
        })
        .collect();
    fast.node().set_paused(false);
    let heights: Vec<u64> = ids
        .iter()
        .map(|id| fast.wait_included(id).block.unwrap().height)
        .collect();
    assert!(
        heights.iter().all(|h| *h == heights[0]),
        "all in the same block: {heights:?}"
    );
    let height = heights[0];
    assert_eq!(fast.ksa(&sink), 20);

    let s = fast.wait_settlement(height, SettlementState::Broadcast);
    let pair = decode_pair(&s);
    assert_eq!(
        pair.commit.output.len(),
        2,
        "outputs independent of the number of users"
    );
    assert_eq!(
        pair.reveal.output.len(),
        2,
        "outputs independent of the number of users"
    );
    assert!(
        pair.commit.input.len() <= 4,
        "inputs are only the producer's funding"
    );
    let payload = ksana_btc::payload::attached_payload(&DEVNET, &pair).unwrap();
    let Payload::Settlement { entries, .. } = Payload::decode(&payload, &DEVNET).unwrap() else {
        panic!("not a settlement payload");
    };
    assert_eq!(entries.len(), 20, "the payload carries 20 entries");
    mine(&btc, 1);
    fast.wait_settlement(height, SettlementState::Confirmed);
    assert_eq!(confirmations(&btc, &txid_of(&s)), 1);
}

/// Acceptance 4: after a native transaction (path C) confirms, it is force-included at the start of the first eligible
/// fast-lane block.
#[test]
fn native_op_is_forced_at_the_start_of_the_first_eligible_block() {
    let btc = Btc::from_env().funded_wallet("m3");
    let carol = user("native/carol");
    let dave = user("native/dave");
    let fast = Fast::start(&btc, "native", &[(&carol, 500)], 2);
    fast.wait_height(2);

    let (tx, op) = btc.fund_native(
        &DEVNET,
        &|input| signed_native(&carol, 0, input, transfer(dave.account, 42)),
        Vec::new(),
    );
    btc.send(&tx);
    let btc_txid = tx.compute_txid().to_byte_array();
    mine(&btc, 1);
    let confirmed_at = btc_tip(&btc).height;

    let status = fast.wait_included(&op.op_id().unwrap());
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    assert_eq!(fast.ksa(&dave), 42);
    let block_ref = status.block.unwrap();
    let block = fast.block(block_ref.height);
    assert_eq!(
        status.index,
        Some(0),
        "the forced-inclusion entry is at the start of the block"
    );
    assert!(
        matches!(block.entries[0].clone(), Entry::Forced { btc_txid: t, op: o } if t == btc_txid && o == op)
    );
    // The first eligible block: h(parent) < confirmation height ≤ h(this block) (with NATIVE_CONFIRMATIONS = 1,
    // h(x) = the anchor height)
    let parent = fast.block(block_ref.height - 1);
    let h = |anchor: u32| i64::from(anchor) - i64::from(DEVNET.native_confirmations) + 1;
    assert!(h(parent.header.btc_anchor.height) < i64::from(confirmed_at));
    assert!(h(block.header.btc_anchor.height) >= i64::from(confirmed_at));
    // A block with only forced-inclusion entries needs no settlement transaction
    if block
        .entries
        .iter()
        .all(|e| matches!(e, Entry::Forced { .. }))
    {
        assert!(block.settlement.is_none());
    }
}

/// Acceptance 5: no spending relationship between settlements -- several settlements in the mempool at the same time
/// do not depend on each other (the funding inputs are all confirmed; within each settlement the reveal transaction
/// depends only on its own commit transaction, D-081), and they confirm in the same BTC block.
#[test]
fn settlements_never_spend_each_other() {
    let btc = Btc::from_env().funded_wallet("m3");
    let erin = user("chain/erin");
    let frank = user("chain/frank");
    let fast = Fast::start(&btc, "chain", &[(&erin, 10_000)], 6);
    fast.wait_height(1);

    // 4 consecutive blocks each with one path-B op, with no BTC block mined meanwhile
    let mut heights = Vec::new();
    for nonce in 0..4 {
        let op = signed(
            &erin,
            nonce,
            FeeMode::Token { amount: 3 },
            transfer(frank.account, 1),
        );
        let id = fast.submit(&op).unwrap();
        heights.push(fast.wait_included(&id).block.unwrap().height);
    }
    heights.dedup();
    assert!(
        heights.len() >= 2,
        "at least two blocks each carry a settlement transaction: {heights:?}"
    );
    let settlements: Vec<SettlementJson> = heights
        .iter()
        .map(|h| fast.wait_settlement(*h, SettlementState::Broadcast))
        .collect();
    let pairs: Vec<SettlementPair> = settlements.iter().map(decode_pair).collect();
    let txids: Vec<Txid> = pairs
        .iter()
        .flat_map(|p| [p.commit.compute_txid(), p.reveal.compute_txid()])
        .collect();
    for pair in &pairs {
        let commit = btc.call(
            "getmempoolentry",
            &[json!(pair.commit.compute_txid().to_string())],
        );
        assert_eq!(
            commit["ancestorcount"], 1,
            "the commit transaction has no unconfirmed ancestor: {commit}"
        );
        assert_eq!(
            commit["descendantcount"], 2,
            "only its own reveal transaction: {commit}"
        );
        let reveal = btc.call(
            "getmempoolentry",
            &[json!(pair.reveal.compute_txid().to_string())],
        );
        assert_eq!(
            reveal["ancestorcount"], 2,
            "depends only on its own commit transaction: {reveal}"
        );
        assert_eq!(
            reveal["descendantcount"], 1,
            "the reveal transaction has no descendant: {reveal}"
        );
        for input in &pair.commit.input {
            assert!(
                !txids.contains(&input.previous_output.txid),
                "a settlement transaction spends another settlement's transaction"
            );
        }
    }
    let block = mine(&btc, 1).pop().unwrap();
    for txid in &txids {
        let info = btc.call("getrawtransaction", &[json!(txid.to_string()), json!(true)]);
        assert_eq!(
            info["blockhash"],
            json!(block),
            "all settlements confirm in the same BTC block"
        );
    }
}

/// Acceptance 6: one CPFP speed-up through the P2A output.
#[test]
fn p2a_anchor_cpfp_confirms_a_stuck_settlement() {
    let btc = Btc::from_env().funded_wallet("m3");
    let gina = user("cpfp/gina");
    let hank = user("cpfp/hank");
    let fast = Fast::start(&btc, "cpfp", &[(&gina, 1_000)], 2);
    fast.wait_height(1);
    let op = signed(
        &gina,
        0,
        FeeMode::Token { amount: 5 },
        transfer(hank.account, 1),
    );
    let id = fast.submit(&op).unwrap();
    let height = fast.wait_included(&id).block.unwrap().height;
    let s = fast.wait_settlement(height, SettlementState::Broadcast);
    let pair = decode_pair(&s);
    let parent = pair.reveal.clone();
    let parent_txid = parent.compute_txid();
    let commit_txid = pair.commit.compute_txid();

    // Simulate a fee spike: lower the mining priority of both settlement transactions; they stay in the mempool after
    // a block is mined
    for txid in [commit_txid, parent_txid] {
        btc.call(
            "prioritisetransaction",
            &[json!(txid.to_string()), json!(0), json!(-100_000)],
        );
    }
    mine(&btc, 1);
    assert!(
        in_mempool(&btc, &parent_txid),
        "the settlement transaction is stuck"
    );
    assert_eq!(confirmations(&btc, &parent_txid), 0);
    assert_eq!(confirmations(&btc, &commit_txid), 0);

    // Anyone: spend the reveal transaction's P2A output (empty witness) and pay enough fee from their own UTXO, which
    // also speeds up the commit transaction (an ancestor)
    let anchor_vout = 0;
    assert_eq!(
        parent.output[anchor_vout].script_pubkey,
        btc_settlement::p2a_script()
    );
    let utxo = btc.call("listunspent", &[json!(1)])[0].clone();
    let wallet_in = BtcOutPoint::new(
        Txid::from_str(utxo["txid"].as_str().unwrap()).unwrap(),
        u32::try_from(utxo["vout"].as_u64().unwrap()).unwrap(),
    );
    let wallet_value = {
        let prev: Transaction = deserialize_hex(
            btc.call("getrawtransaction", &[json!(wallet_in.txid.to_string())])
                .as_str()
                .unwrap(),
        )
        .unwrap();
        prev.output[wallet_in.vout as usize].value.to_sat()
    };
    let child_fee = 300_000;
    let change_addr = btc.new_address();
    let change_script = bitcoin::Address::from_str(&change_addr)
        .unwrap()
        .assume_checked()
        .script_pubkey();
    let child = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![
            TxIn {
                previous_output: BtcOutPoint::new(parent_txid, u32::try_from(anchor_vout).unwrap()),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            },
            TxIn {
                previous_output: wallet_in,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            },
        ],
        output: vec![TxOut {
            value: Amount::from_sat(wallet_value + btc_settlement::P2A_AMOUNT_SAT - child_fee),
            script_pubkey: change_script,
        }],
    };
    // The wallet can only sign its own input; the P2A input needs no signature (empty witness)
    let signed_child = btc.call(
        "signrawtransactionwithwallet",
        &[json!(serialize_hex(&child))],
    );
    let child: Transaction = deserialize_hex(signed_child["hex"].as_str().unwrap()).unwrap();
    assert!(child.input[0].witness.is_empty());
    btc.send(&child);
    let block = mine(&btc, 1).pop().unwrap();
    for txid in [commit_txid, parent_txid, child.compute_txid()] {
        let info = btc.call("getrawtransaction", &[json!(txid.to_string()), json!(true)]);
        assert_eq!(
            info["blockhash"],
            json!(block),
            "commit, reveal and child transactions confirm in the same block"
        );
    }
    fast.wait_settlement(height, SettlementState::Confirmed);
}

/// A block's acceptance position on the main chain (D-079): the height of the main-chain block accepting it and its
/// index in the acceptance order.
fn position(fast: &Fast, block_hash: &[u8; 32]) -> (u64, u32) {
    let main = fast.node().chain().main_chain().unwrap();
    let found = main.iter().find_map(|(height, entry)| {
        entry
            .accepted()
            .position(|b| b.hash == *block_hash)
            .map(|i| (height, u32::try_from(i).unwrap()))
    });
    found.expect("the block has been accepted by the main chain")
}

/// Uses the node's executor to try including, on top of the main-chain tip, an entry unlocking up to `block_hash`'s
/// position (the same code as block validation): returns whether it is accepted.
fn unlock_accepted(fast: &Fast, block_hash: [u8; 32]) -> Result<(), String> {
    let (target_height, index) = position(fast, &block_hash);
    let chain = fast.node().chain();
    let (tip, view) = chain.tip_and_view().unwrap();
    let snapshot = chain.btc().snapshot().unwrap();
    let (height, hash) = snapshot.tip().unwrap();
    let anchor = BtcAnchor { height, hash };
    let btc: Arc<dyn BtcView> = Arc::new(chain.btc_view(anchor.clone(), snapshot, false).unwrap());
    let header = ksana_primitives::BlockHeader {
        version: ksana_primitives::BLOCK_VERSION,
        height: tip.header.height + 1,
        parents: vec![tip.hash],
        btc_anchor: anchor,
        entries_root: [0; 32],
        state_root: [0; 32],
        producer: fast.producer.account,
        timestamp: tip.header.timestamp + 1,
        bits: tip.header.bits,
        nonce: 0,
    };
    let daa_score = chain.ghostdag(&tip.hash).unwrap().unwrap().daa_score + 1;
    let env = chain.block_env(&header, daa_score, &view).unwrap();
    let entry = Entry::Unlock {
        height: target_height,
        index,
    };
    match chain
        .executor()
        .build_block(Arc::new(view), btc, &env, |b| b.push(&entry).map(|_| ()))
    {
        Ok(_) => Ok(()),
        Err(BlockError::Invalid(m)) => Err(m),
        Err(BlockError::Fatal(m)) => panic!("local fault: {m}"),
    }
}

/// Acceptance 7: unlocking is possible only after the settlement is confirmed and in the settled prefix; after
/// unlocking, the token fees (together with the block reward, D-074) become available balance; no double unlock.
/// Unlocking is cumulative (D-074, D-079): the producer writes one entry unlocking, in acceptance order, up to the
/// last settled position; the records are deleted right away and the rewards are paid at unlock.
#[test]
fn unlock_requires_a_confirmed_settlement_inside_the_settled_prefix() {
    let btc = Btc::from_env().funded_wallet("m3");
    let ivy = user("unlock/ivy");
    let jack = user("unlock/jack");
    let fast = Fast::start(&btc, "unlock", &[(&ivy, 10_000)], 4);
    fast.wait_height(1);

    // Two blocks X1 < X2 each with one path-B op (sponsored fees 11 and 13)
    let mut blocks = Vec::new();
    for (nonce, fee) in [(0u64, 11u128), (1, 13)] {
        let op = signed(
            &ivy,
            nonce,
            FeeMode::Token { amount: fee },
            transfer(jack.account, 1),
        );
        let id = fast.submit(&op).unwrap();
        let block = fast.wait_included(&id).block.unwrap();
        blocks.push((block, fee));
    }
    let (x1, fee1) = blocks[0].clone();
    let (x2, fee2) = blocks[1].clone();
    assert!(x1.height < x2.height);
    let s1 = fast.wait_settlement(x1.height, SettlementState::Broadcast);
    let s2 = fast.wait_settlement(x2.height, SettlementState::Broadcast);
    // The sponsored fee is recorded at the block's acceptance position when received; the RPC's locked balance adds
    // the block reward that will be paid at unlock (D-074, D-079)
    let reward = DEVNET.block_reward;
    fast.wait_height(x2.height + 1);
    assert_eq!(fast.locked(&fast.producer, &x1.hash), Some(fee1 + reward));
    assert_eq!(fast.ksa(&fast.producer), 0);

    // Settlement unconfirmed: cannot unlock
    let err = unlock_accepted(&fast, x1.hash).unwrap_err();
    assert!(!err.is_empty());

    // Confirm only X2's settlement (X1's stays in the mempool with lowered priority): its payload counts, but X2's
    // header does not, since X1's has not (D-135): it lands nothing, X2 is not in the settled prefix, and it still
    // cannot unlock
    btc.call(
        "prioritisetransaction",
        &[json!(txid_of(&s1).to_string()), json!(0), json!(-100_000)],
    );
    mine(&btc, 1);
    let s2_txid = s2.txid.expect("the block carries settlement transactions");
    wait("X2's settlement to count", Duration::from_secs(60), || {
        let snapshot = fast.node().chain().btc().snapshot().ok()?;
        snapshot.ksn1_height(&s2_txid).map(|_| ())
    });
    let snapshot = fast.node().chain().btc().snapshot().unwrap();
    assert_eq!(snapshot.counted_at(&x2.hash), None);
    for x in [&x1, &x2] {
        let s = fast.settlement(x.height);
        assert_eq!(s.status, SettlementState::Broadcast);
        assert!(!s.settled);
    }
    unlock_accepted(&fast, x2.hash).unwrap_err();
    let before = fast.tip_height();
    fast.wait_height(before + 3);
    assert_eq!(
        fast.locked(&fast.producer, &x2.hash),
        Some(fee2 + reward),
        "the producer does not write an invalid unlock entry"
    );

    // After X1's settlement confirms, X1 lands, and the node sends X2's payload again, unchanged (D-135); once that
    // confirms, both blocks are in the settled prefix, the producer writes unlock entries up to X2, and the token fees
    // become available balance
    btc.call(
        "prioritisetransaction",
        &[json!(txid_of(&s1).to_string()), json!(0), json!(100_000)],
    );
    mine(&btc, 1);
    fast.wait_settlement(x1.height, SettlementState::Confirmed);
    let payload = attached_payload(&DEVNET, &decode_pair(&s2)).unwrap();
    let reveal = wait("X2's payload sent again", Duration::from_secs(60), || {
        payload_in_mempool(&btc, &payload, &txid_of(&s2))
    });
    // Only that pair: the settlement of a block unlocking up to X1 meanwhile keeps waiting, so that nothing after X2
    // lands
    let commit = reveal.input[0].previous_output.txid;
    btc.call(
        "generateblock",
        &[
            json!(btc.new_address()),
            json!([commit.to_string(), reveal.compute_txid().to_string()]),
        ],
    );
    let s2 = fast.wait_settlement(x2.height, SettlementState::Confirmed);
    assert_eq!(
        s2.confirmed.map(|c| c.txid),
        Some(reveal.compute_txid().to_byte_array())
    );
    wait("the unlock", Duration::from_secs(60), || {
        (fast.locked(&fast.producer, &x1.hash).is_none()
            && fast.locked(&fast.producer, &x2.hash).is_none())
        .then_some(())
    });
    // The headers of the empty blocks before X2 are carried by X1's and X2's settlement payloads: the unlock entries
    // reach X2, and the sponsored fees are unlocked together with the block rewards of blocks 1-X2; each block's
    // reward is weighted (an empty block counts 12, 24, ... shares by streak, D-084, D-090; on a single producer's
    // linear chain the streak carries over from the previous block)
    let mut streak = 0;
    let mut rewards: u128 = 0;
    for h in 1..=x2.height {
        let header = fast.block(h).header;
        rewards += u128::from(DEVNET.block_weight(&header, streak)) * reward;
        streak = DEVNET.block_streak(&header, streak);
    }
    assert_eq!(fast.ksa(&fast.producer), fee1 + fee2 + rewards);
    let tip = fast.tip_height();
    let unlocks: Vec<(u64, u32)> = (x2.height + 1..=tip)
        .flat_map(|h| fast.block(h).entries)
        .filter_map(|e| match e {
            Entry::Unlock { height, index } => Some((height, index)),
            _ => None,
        })
        .collect();
    // X1 lands a BTC block before X2: the producer may unlock up to X1 in between, then up to X2
    let last = position(&fast, &x2.hash);
    assert!(
        unlocks == vec![last] || unlocks == vec![position(&fast, &x1.hash), last],
        "{unlocks:?}"
    );
    // The same block cannot be unlocked twice
    let again = unlock_accepted(&fast, x1.hash).unwrap_err();
    assert!(!again.is_empty());
}
