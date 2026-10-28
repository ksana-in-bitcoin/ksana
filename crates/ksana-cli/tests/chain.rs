//! The CLI signs for the node's network (A2-11, D-106): every command but `key` first reads the node's chain ID,
//! refuses a node on another network than `--chain` or on no Ksana network, and path C and top-up refuse a bitcoind on
//! another BTC network, all before anything is signed, funded or sent. A path C op signed with another network's chain
//! ID is still included by the node, and fails there while its BTC payment confirms.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use jsonrpsee::core::RpcResult;
use jsonrpsee::server::{RpcModule, Server, ServerHandle};
use jsonrpsee::types::ErrorObjectOwned;
use ksana_primitives::{Network, VmOp, SIGNET};
use ksana_rpc::{
    codes, error, start_server, AccountJson, AssetJson, AssetStatusJson, Backend, BlockId,
    BlockJson, BlockRef, BlockSummary, CallRequestJson, ChainInfoJson, Cors, EstimateFeeJson,
    OpListJson, OpPosition, OpStatusJson, ReceiptJson, SettlementJson, SubmitResult,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// The buyer's secret key in the top-up case, and the signer's elsewhere (any valid secret key).
const KEY: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const EXCHANGE: &str = "e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1";

/// A node on chain ID `chain_id` that records the ops submitted to it and has one open lock (`get_lock` of any
/// exchange contract), whose beneficiary is `beneficiary`.
struct MockNode {
    chain_id: u32,
    beneficiary: [u8; 32],
    submitted: Mutex<Vec<Vec<u8>>>,
    blocks: broadcast::Sender<BlockSummary>,
}

fn absent<T>() -> RpcResult<T> {
    Err(error(codes::NOT_FOUND, "not found"))
}

fn genesis() -> BlockRef {
    BlockRef {
        height: 0,
        hash: [0x9e; 32],
    }
}

impl Backend for MockNode {
    fn submit_op(&self, op: Vec<u8>) -> RpcResult<SubmitResult> {
        self.submitted.lock().unwrap().push(op);
        Ok(SubmitResult { op_id: [0; 32] })
    }
    fn op_status(&self, _: [u8; 32]) -> RpcResult<OpStatusJson> {
        absent()
    }
    fn block(&self, _: BlockId) -> RpcResult<BlockJson> {
        absent()
    }
    fn account(&self, account: [u8; 32]) -> RpcResult<AccountJson> {
        Ok(AccountJson {
            account,
            nonce: 0,
            balances: Vec::new(),
            locked_fees: Vec::new(),
            at: genesis(),
        })
    }
    fn asset(&self, _: [u8; 32]) -> RpcResult<AssetJson> {
        absent()
    }
    fn asset_status(&self, _: [u8; 32], _: [u8; 32]) -> RpcResult<AssetStatusJson> {
        absent()
    }
    fn call(&self, req: CallRequestJson) -> RpcResult<ReceiptJson> {
        if req.method != "get_lock" {
            return absent();
        }
        // Lock { seller, beneficiary, script, price, ksa, expiry, state: Open } of `ksa-exchange`
        let lock = borsh::to_vec(&(
            [0x5e_u8; 32],
            self.beneficiary,
            vec![0x00_u8, 0x14, 0x11, 0x22],
            100_u128,
            3_000_000_u128,
            1_000_000_u32,
            0_u8,
        ))
        .unwrap();
        Ok(ReceiptJson {
            op_id: None,
            success: true,
            error: None,
            gas_used: 0,
            output: lock,
            events: Vec::new(),
            asset_changes: Some(Vec::new()),
        })
    }
    fn settlement(&self, _: BlockId) -> RpcResult<SettlementJson> {
        absent()
    }
    fn estimate_fee(&self) -> RpcResult<EstimateFeeJson> {
        absent()
    }
    fn chain_info(&self) -> RpcResult<ChainInfoJson> {
        Ok(ChainInfoJson {
            chain_id: self.chain_id,
            tip: genesis(),
            daa_score: 0,
            protocol_version: 2,
            next_upgrade: None,
            max_known_version: 2,
            finalized: genesis(),
        })
    }
    fn account_ops(&self, _: [u8; 32], _: Option<OpPosition>, _: usize) -> RpcResult<OpListJson> {
        absent()
    }
    fn latest_ops(&self, _: Option<OpPosition>, _: usize) -> RpcResult<OpListJson> {
        absent()
    }
    fn subscribe_blocks(&self) -> broadcast::Receiver<BlockSummary> {
        self.blocks.subscribe()
    }
}

/// A bitcoind on BTC `chain` that records the methods called; it never funds a transaction.
struct FakeBitcoind {
    chain: &'static str,
    calls: Mutex<Vec<&'static str>>,
}

/// The node and bitcoind of one case, served until dropped.
struct Services {
    node: Arc<MockNode>,
    node_url: String,
    bitcoind: Arc<FakeBitcoind>,
    bitcoind_url: String,
    handles: Vec<ServerHandle>,
    runtime: tokio::runtime::Runtime,
}

impl Drop for Services {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            let _ = handle.stop();
            self.runtime.block_on(handle.stopped());
        }
    }
}

fn start(chain_id: u32, btc_chain: &'static str) -> Services {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let keypair = bitcoin::key::Keypair::from_seckey_slice(
        &bitcoin::key::Secp256k1::new(),
        &hex::decode(KEY).unwrap(),
    )
    .unwrap();
    let node = Arc::new(MockNode {
        chain_id,
        beneficiary: ksana_crypto::account_of(&keypair),
        submitted: Mutex::new(Vec::new()),
        blocks: broadcast::channel(1).0,
    });
    let backend: Arc<dyn Backend> = node.clone();
    let (node_addr, node_handle) = runtime
        .block_on(start_server(
            "127.0.0.1:0".parse().unwrap(),
            backend,
            &Cors::Disabled,
        ))
        .unwrap();

    let bitcoind = Arc::new(FakeBitcoind {
        chain: btc_chain,
        calls: Mutex::new(Vec::new()),
    });
    let mut module = RpcModule::new(bitcoind.clone());
    module
        .register_method("getblockchaininfo", |_, b, _| {
            b.calls.lock().unwrap().push("getblockchaininfo");
            Ok::<Value, ErrorObjectOwned>(json!({ "chain": b.chain, "blocks": 200 }))
        })
        .unwrap();
    module
        .register_method("getblockcount", |_, b, _| {
            b.calls.lock().unwrap().push("getblockcount");
            Ok::<Value, ErrorObjectOwned>(json!(200))
        })
        .unwrap();
    for method in [
        "fundrawtransaction",
        "signrawtransactionwithwallet",
        "sendrawtransaction",
    ] {
        module
            .register_method(method, move |_, b, _| {
                b.calls.lock().unwrap().push(method);
                Err::<Value, _>(ErrorObjectOwned::owned(
                    -4,
                    "Insufficient funds",
                    None::<()>,
                ))
            })
            .unwrap();
    }
    let (bitcoind_addr, bitcoind_handle) = runtime.block_on(async {
        let server = Server::builder()
            .build("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        (addr, server.start(module))
    });

    Services {
        node,
        node_url: format!("http://{node_addr}"),
        bitcoind,
        bitcoind_url: format!("http://{bitcoind_addr}"),
        handles: vec![node_handle, bitcoind_handle],
        runtime,
    }
}

impl Services {
    /// Runs `ksana-cli --rpc <node> <args>` with the key file `key`.
    fn cli(&self, key: &Path, args: &[&str]) -> Output {
        let args: Vec<String> = args
            .iter()
            .map(|a| match *a {
                "<key>" => key.display().to_string(),
                "<btc>" => self.bitcoind_url.clone(),
                a => a.to_string(),
            })
            .collect();
        Command::new(env!("CARGO_BIN_EXE_ksana-cli"))
            .arg("--rpc")
            .arg(&self.node_url)
            .args(&args)
            .env_remove("KSANA_RPC")
            .env_remove("KSANA_BTC_RPC_URL")
            .output()
            .unwrap()
    }

    fn submitted(&self) -> Vec<Vec<u8>> {
        self.node.submitted.lock().unwrap().clone()
    }

    fn btc_calls(&self) -> Vec<&'static str> {
        self.bitcoind.calls.lock().unwrap().clone()
    }
}

fn key_file(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("signer.key");
    std::fs::write(&path, KEY).unwrap();
    path
}

/// The account of [`KEY`] as a Taproot address of `network`.
fn address(network: bitcoin::Network) -> String {
    let keypair = bitcoin::key::Keypair::from_seckey_slice(
        &bitcoin::key::Secp256k1::new(),
        &hex::decode(KEY).unwrap(),
    )
    .unwrap();
    ksana_crypto::account_address(&ksana_crypto::account_of(&keypair), network)
        .unwrap()
        .to_string()
}

fn refused(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "expected a refusal, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn succeeded(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "ksana-cli failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

const TRANSFER: [&str; 11] = [
    "transfer",
    "--key",
    "<key>",
    "--to",
    "1111111111111111111111111111111111111111111111111111111111111111",
    "--amount",
    "5",
    "--fee-token",
    "1",
    "--nonce",
    "0",
];

#[test]
fn a_node_on_another_network_than_chain_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    let services = start(SIGNET.chain_id, "signet");
    for expected in ["devnet", "mainnet"] {
        let mut args = vec!["--chain", expected];
        args.extend(TRANSFER);
        let stderr = refused(&services.cli(&key, &args));
        assert!(stderr.contains("signet"), "{stderr}");
        assert!(stderr.contains("--chain"), "{stderr}");
        // queries are refused too
        let account = "11".repeat(32);
        let stderr = refused(&services.cli(&key, &["--chain", expected, "account", &account]));
        assert!(stderr.contains("--chain"), "{stderr}");
    }
    assert!(services.submitted().is_empty(), "nothing reaches the node");
}

#[test]
fn a_node_on_no_ksana_network_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    let services = start(4242, "regtest");
    let stderr = refused(&services.cli(&key, &TRANSFER));
    assert!(stderr.contains("4242"), "{stderr}");
    assert!(services.submitted().is_empty());
}

#[test]
fn ops_are_signed_for_the_nodes_network() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    let services = start(SIGNET.chain_id, "signet");
    // without --chain the network is the node's; with a matching --chain the same
    let mut with_chain = vec!["--chain", "signet"];
    with_chain.extend(TRANSFER);
    for args in [TRANSFER.to_vec(), with_chain] {
        succeeded(&services.cli(&key, &args));
    }
    let submitted = services.submitted();
    assert_eq!(submitted.len(), 2);
    for bytes in submitted {
        let op = VmOp::decode(&bytes, &SIGNET).unwrap();
        assert_eq!(op.chain_id, SIGNET.chain_id);
    }
    // account arguments take the node's BTC network: a signet address is accepted, a regtest one refused
    let signet = address(bitcoin::Network::Signet);
    let info = succeeded(&services.cli(&key, &["account", &signet]));
    assert_eq!(info["address"], json!(signet));
    let stderr = refused(&services.cli(&key, &["account", &address(bitcoin::Network::Regtest)]));
    assert!(stderr.contains("invalid account"), "{stderr}");
    // `key` does not talk to the node: it stays offline (devnet addresses unless --chain says otherwise)
    let offline = Command::new(env!("CARGO_BIN_EXE_ksana-cli"))
        .args(["--rpc", "http://127.0.0.1:1", "--chain", "signet", "key"])
        .arg(&key)
        .output()
        .unwrap();
    assert_eq!(succeeded(&offline)["address"], json!(signet));
}

#[test]
fn path_c_refuses_a_bitcoind_on_another_btc_network() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    // a signet node with a regtest bitcoind: the transaction would never reach the node
    let services = start(SIGNET.chain_id, "regtest");
    let args = [
        "transfer",
        "--key",
        "<key>",
        "--to",
        "1111111111111111111111111111111111111111111111111111111111111111",
        "--amount",
        "5",
        "--native",
        "--nonce",
        "0",
        "--btc-rpc-url",
        "<btc>",
    ];
    let stderr = refused(&services.cli(&key, &args));
    assert!(stderr.contains("regtest"), "{stderr}");
    assert!(stderr.contains("signet"), "{stderr}");
    assert_eq!(
        services.btc_calls(),
        ["getblockchaininfo"],
        "nothing funded"
    );
    assert!(services.submitted().is_empty());
}

#[test]
fn top_up_refuses_a_bitcoind_on_another_btc_network() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    let services = start(SIGNET.chain_id, "regtest");
    let beneficiary = address(bitcoin::Network::Signet);
    let args = [
        "top-up",
        "--exchange",
        EXCHANGE,
        "--lock",
        "0",
        "--beneficiary",
        &beneficiary,
        "--sats",
        "30000",
        "--btc-rpc-url",
        "<btc>",
    ];
    let stderr = refused(&services.cli(&key, &args));
    assert!(stderr.contains("regtest"), "{stderr}");
    assert_eq!(
        services.btc_calls(),
        ["getblockchaininfo"],
        "no BTC height read, nothing funded or paid"
    );
}

#[test]
fn a_bitcoind_on_the_nodes_btc_network_goes_on_to_funding() {
    let dir = tempfile::tempdir().unwrap();
    let key = key_file(dir.path());
    let services = start(Network::Devnet.params().chain_id, "regtest");
    let beneficiary = address(bitcoin::Network::Regtest);
    let args = [
        "top-up",
        "--exchange",
        EXCHANGE,
        "--lock",
        "0",
        "--beneficiary",
        &beneficiary,
        "--sats",
        "30000",
        "--btc-rpc-url",
        "<btc>",
    ];
    // the fake bitcoind has no funds: the top-up gets as far as funding
    let stderr = refused(&services.cli(&key, &args));
    assert!(stderr.contains("fundrawtransaction"), "{stderr}");
    assert_eq!(
        services.btc_calls(),
        [
            "getblockchaininfo",
            "getblockcount",
            "getblockchaininfo",
            "fundrawtransaction"
        ]
    );
}
