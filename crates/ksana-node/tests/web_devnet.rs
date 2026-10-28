//! Local devnet for web page integration (ignored by default, not run under `cargo test`): needs neither docker nor
//! bitcoind.
//!
//! Two producing nodes connected over libp2p share one in-memory mock BTC chain (`MockBtc`); a fake bitcoind exposes
//! this chain over JSON-RPC to `ksana-indexer`; there is also periodic BTC block mining and path-B transfer traffic.
//! The node RPC allows cross-origin access from any origin (the equivalent of `--rpc-cors '*'`).
//!
//! ```sh
//! cargo build -p ksana-indexer
//! cargo test -p ksana-node --test web_devnet -- --ignored --nocapture
//! # In another terminal, in a checkout of ksana-web (github.com/ksana-in-bitcoin/ksana-web):
//! KSANA_RPC=http://127.0.0.1:18545 KSANA_INDEXER=http://127.0.0.1:18548 KSANA_BTC=<fake bitcoind address printed below> npm run dev
//! # Open http://localhost:5173/?rpc=/rpc&indexer=/indexer&btc=/btc
//! ```
//!
//! Ports: nodes 18545, 18546, indexer 18548. alice's secret key and address are printed to stderr; the Taproot
//! addresses (i.e. accounts, D-092) of alice and bob each hold 10 confirmed UTXOs of 150,000 sats, so the web page can
//! take path B and path C with the test secret-key wallet (`?wallet=mock&key=<secret key>`). Environment variables:
//! `WEB_DEVNET_DIR` (data directory), `WEB_DEVNET_NODES` (number of nodes, default 2), `WEB_DEVNET_BLOCK_MS` (minimum
//! block interval, default 3000), `WEB_DEVNET_BTC_MS` (BTC block interval, default 9000), `WEB_DEVNET_TRAFFIC_MS`
//! (transfer interval, default 1200), `KSANA_INDEXER_BIN` (indexer executable, default `target/debug/ksana-indexer`).

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::Network;
use common::{fake_bitcoind, transfer_op, user};
use jsonrpsee::http_client::HttpClientBuilder;
use ksana_btc::client::BtcClient;
use ksana_btc::mock::MockBtc;
use ksana_crypto::account::account_address;
use ksana_mempool::Policy;
use ksana_node::genesis::{Allocation, GenesisFile};
use ksana_node::node::{NetConfig, Node, NodeConfig};
use ksana_node::producer::Funding;
use ksana_p2p::{Multiaddr, Tuning};
use ksana_primitives::hash::sha256;
use ksana_primitives::{BtcAnchor, DEVNET};
use ksana_rpc::KsanaApiClient;

/// The indexer child process: terminated together with the test process when it exits.
struct Indexer(Child);

impl Drop for Indexer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "local integration tool: runs continuously, started manually"]
fn web_devnet() {
    let base = PathBuf::from(
        std::env::var("WEB_DEVNET_DIR").unwrap_or_else(|_| "/tmp/ksana-web-devnet".into()),
    );
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let mock = Arc::new(MockBtc::new());
    mock.mine(1);
    let secp = Secp256k1::new();
    let names = ["alice", "bob", "carol", "dave"];
    let users: Vec<_> = names.iter().map(|n| user(n)).collect();
    // Put some confirmed UTXOs on the Taproot addresses (i.e. the accounts, D-092) of alice and bob for the web page to
    // take path C and top up
    for (name, u) in names.iter().zip(&users).take(2) {
        let secret =
            SecretKey::from_slice(&sha256(format!("ksana/node-test/{name}").as_bytes())).unwrap();
        let address = account_address(&u.account, Network::Regtest).unwrap();
        let outpoints = mock.fund(&address.script_pubkey(), &[150_000; 10]);
        eprintln!(
            "{name} secret = {} account = {} address = {address}",
            hex::encode(secret.secret_bytes()),
            hex::encode(u.account)
        );
        // Can be entered manually when the web page has no Bitcoin RPC configured: txid:vout:sats
        for o in outpoints.iter().take(3) {
            eprintln!("  utxo {}:{}:150000", o.txid, o.vout);
        }
    }
    let n_nodes = usize::try_from(env_u64("WEB_DEVNET_NODES", 2)).unwrap();
    let fundings: Vec<Funding> = (0..n_nodes)
        .map(|i| {
            let secret =
                SecretKey::from_slice(&sha256(format!("web/funding/{i}").as_bytes())).unwrap();
            let funding = Funding::new(Keypair::from_secret_key(&secp, &secret), 2);
            mock.fund(&funding.script, &[2_000_000; 30]);
            funding
        })
        .collect();
    let (height, hash) = mock.tip().unwrap();
    let genesis = GenesisFile {
        timestamp: ksana_node::unix_now(),
        btc_anchor: BtcAnchor { height, hash },
        bits: None,
        block_share_account: None,
        allocations: users
            .iter()
            .zip([1_000_000_000u128, 80_000_000, 30_000_000, 5_000_000])
            .map(|(u, amount)| Allocation {
                account: u.account,
                amount,
            })
            .collect(),
    };
    let genesis_path = base.join("genesis.json");
    std::fs::write(
        &genesis_path,
        serde_json::to_string_pretty(&genesis).unwrap(),
    )
    .unwrap();

    let interval = Duration::from_millis(env_u64("WEB_DEVNET_BLOCK_MS", 3000));
    // The nodes run until the process exits
    let mut nodes: Vec<Node> = Vec::new();
    let mut addrs: Vec<Multiaddr> = Vec::new();
    for (i, funding) in fundings.into_iter().enumerate() {
        let keypair =
            ksana_p2p::Keypair::ed25519_from_bytes(sha256(format!("web/p2p/{i}").as_bytes()))
                .unwrap();
        let node = Node::start(NodeConfig {
            data_dir: base.join(format!("node{i}")),
            genesis: genesis.clone(),
            params: DEVNET,
            rpc_addr: format!("127.0.0.1:{}", 18545 + i).parse().unwrap(),
            rpc_cors: ksana_rpc::Cors::Any,
            producer: Some(user(&format!("web/producer/{i}")).account),
            carrier_payee: None,
            funding: Some(funding),
            policy: Policy::default(),
            btc: mock.clone(),
            btc_poll: Duration::from_millis(200),
            min_block_interval: interval,
            net: Some(NetConfig {
                keypair,
                listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                peers: addrs.clone(),
                tuning: Tuning::default(),
            }),
            pow_algorithm: None,
            pruning: ksana_node::chain::Pruning::default(),
        })
        .unwrap();
        let network = node.network().unwrap();
        let addr: Multiaddr = format!(
            "{}/p2p/{}",
            network.listen_addrs()[0],
            network.local_peer_id()
        )
        .parse()
        .unwrap();
        addrs.push(addr);
        eprintln!("node{i} rpc = http://{}", node.rpc_addr());
        nodes.push(node);
    }

    let fake = fake_bitcoind::start(mock.clone());
    eprintln!(
        "fake bitcoind = {} (web page: KSANA_BTC={} npm run dev, then add &btc=/btc)",
        fake.url, fake.url
    );
    let indexer_bin = std::env::var("KSANA_INDEXER_BIN").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/debug/ksana-indexer"
        )
        .into()
    });
    let mut indexer = Indexer(
        Command::new(indexer_bin)
            .arg("--data-dir")
            .arg(base.join("indexer"))
            .arg("--genesis")
            .arg(&genesis_path)
            .arg("--btc-rpc-url")
            .arg(&fake.url)
            .arg("--rpc-addr")
            .arg("127.0.0.1:18548")
            .arg("--poll-ms")
            .arg("300")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect(
                "run cargo build -p ksana-indexer first, or point KSANA_INDEXER_BIN at the indexer",
            ),
    );
    let mut line = String::new();
    BufReader::new(indexer.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    eprintln!("indexer: {}", line.trim());
    let stderr = indexer.0.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("  indexer | {line}");
        }
    });

    // BTC block mining
    let btc_ms = env_u64("WEB_DEVNET_BTC_MS", 9000);
    let miner = mock.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(btc_ms));
        miner.mine(1);
    });

    // Path-B transfer traffic (submitted via the node RPC, same as a wallet): bob and carol take turns sending, alice
    // is left for the web wallet
    let rt = common::rt();
    let clients: Vec<_> = (0..n_nodes)
        .map(|i| {
            HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .build(format!("http://127.0.0.1:{}", 18545 + i))
                .unwrap()
        })
        .collect();
    let traffic_ms = env_u64("WEB_DEVNET_TRAFFIC_MS", 1200);
    let mut nonces = vec![None::<u64>; users.len()];
    let mut k = 0u64;
    loop {
        k += 1;
        std::thread::sleep(Duration::from_millis(traffic_ms));
        let from = if k.is_multiple_of(2) { 1 } else { 2 };
        let to = [0usize, 1, 2, 3][usize::try_from((k * 7 + 3) % 4).unwrap()];
        if to == from {
            continue;
        }
        let client = &clients[usize::try_from(k).unwrap() % clients.len()];
        let u = &users[from];
        let nonce = match nonces[from] {
            Some(n) => n,
            None => match rt.block_on(client.get_account(hex::encode(u.account))) {
                Ok(acc) => acc.nonce,
                Err(e) => {
                    eprintln!("getAccount failed: {e}");
                    continue;
                }
            },
        };
        let fee = [1u128, 1, 2, 3, 5][usize::try_from(k % 5).unwrap()];
        let amount = 1000 + u128::from(k % 17) * 250;
        let op = transfer_op(u, nonce, fee, users[to].account, amount);
        match rt.block_on(client.submit_op(hex::encode(op.encode().unwrap()))) {
            Ok(_) => {
                // Print the op_id for searching on the web page, or as the recording parameter of the replay fixture
                // (KSANA_RECORD_OP)
                eprintln!(
                    "op {} {} -> {} nonce {nonce} fee {fee} amount {amount}",
                    hex::encode(op.op_id().unwrap()),
                    names[from],
                    names[to]
                );
                nonces[from] = Some(nonce + 1);
            }
            Err(e) => {
                eprintln!("submitOp failed ({}): {e}", names[from]);
                nonces[from] = None;
            }
        }
    }
}
