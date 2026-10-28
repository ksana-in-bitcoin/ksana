//! The `ksana-indexer` binary: connects to a fake bitcoind through the real bitcoind RPC client (a bitcoind-compatible
//! JSON-RPC over the in-memory mock chain, `crates/ksana-node/tests/common/fake_bitcoind.rs` shared with the node's
//! subprocess tests), prints the RPC address on stdout, verifies up to `SETTLEMENT_DEPTH` blocks before the node's
//! settled prefix (D-063), and exits normally on SIGTERM.

mod common;
#[allow(dead_code)]
#[path = "../../ksana-node/tests/common/fake_bitcoind.rs"]
mod fake_bitcoind;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use ksana_indexer::rpc::{IndexerApiClient, StateRootJson, StatusJson};
use ksana_primitives::DEVNET;

#[test]
fn binary_indexes_and_exits_on_sigterm() {
    let (alice, bob) = (user("bin/alice"), user("bin/bob"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    // The binary runs with the `--chain devnet` params; the node side uses DEVNET too
    let a = Side::with_params(&mock, &genesis, "bin", DEVNET);
    let depth = DEVNET.settlement_depth;
    a.submit(transfer(&alice, 0, token(3), bob.account, 10));
    a.mine();
    for _ in 0..depth {
        a.mine_empty();
    }
    a.broadcast_settlements();
    mock.mine(1);
    // The settlement of block depth + 2 carries the headers from block 2 on: the indexer's main chain reaches this
    // block, and the one with SETTLEMENT_DEPTH confirmations is block 2
    a.submit(transfer(&alice, 1, token(3), bob.account, 10));
    a.mine();
    for _ in 0..depth {
        a.mine_empty();
    }
    a.broadcast_settlements();
    mock.mine(1);
    let expected = a.block(2);

    let fake = fake_bitcoind::start(mock.clone());
    let dir = tempfile::tempdir().unwrap();
    let genesis_path = dir.path().join("genesis.json");
    std::fs::write(
        &genesis_path,
        serde_json::to_string_pretty(&genesis).unwrap(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ksana-indexer"))
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .arg("--genesis")
        .arg(&genesis_path)
        .arg("--btc-rpc-url")
        .arg(&fake.url)
        .arg("--rpc-addr")
        .arg("127.0.0.1:0")
        .arg("--poll-ms")
        .arg("100")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let addr = line
        .trim()
        .strip_prefix("ksana-indexer rpc=")
        .unwrap_or_else(|| panic!("unexpected output: {line:?}"))
        .to_string();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("  | {line}");
        }
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = jsonrpsee::http_client::HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(30))
        .build(format!("http://{addr}"))
        .unwrap();
    let started = ksana_node::monotonic_now();
    let status: StatusJson = loop {
        let status: StatusJson = rt.block_on(client.status()).unwrap();
        if status.height >= 2 {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "timed out waiting for the indexer to catch up: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status.height, 2);
    assert_eq!(status.hash, expected.header.hash());
    assert_eq!(status.state_root, expected.header.state_root);
    assert_eq!(status.halted, None);
    let root: StateRootJson = rt.block_on(client.state_root("2".into())).unwrap();
    assert_eq!(root.state_root, expected.header.state_root);

    let killed = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(killed.success());
    let exit = child.wait().unwrap();
    assert!(exit.success(), "the indexer did not exit normally: {exit}");
}
