//! SPEC §16 "secret keys exist only in wallets and locally at the producer; logs must not output any key material":
//! the output of the key tool and the node binary contains no secret key.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ksana_node::genesis::GenesisFile;
use ksana_primitives::BtcAnchor;

/// Runs the command (at most 60 seconds) and returns the full contents of stdout and stderr.
fn run(command: &mut Command) -> String {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = ksana_node::monotonic_now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(60) {
            child.kill().unwrap();
            panic!("command did not exit within 60 seconds: {command:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn secret_of(path: &Path) -> String {
    let secret = std::fs::read_to_string(path).unwrap().trim().to_lowercase();
    assert_eq!(secret.len(), 64);
    secret
}

fn assert_no_secret(output: &str, secret: &str) {
    assert!(
        !output.to_lowercase().contains(secret),
        "output contains the secret key:\n{output}"
    );
}

#[test]
fn keytool_and_node_never_print_the_secret_key() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("funding.key");
    // Generate the secret key file, then read it once more: only the public key, script and address are printed
    let created = run(Command::new(env!("CARGO_BIN_EXE_ksana-keytool")).arg(&key));
    let secret = secret_of(&key);
    let again = run(Command::new(env!("CARGO_BIN_EXE_ksana-keytool")).arg(&key));
    for output in [&created, &again] {
        assert!(output.contains("regtest_address"), "{output}");
        assert_no_secret(output, &secret);
    }
    let info: serde_json::Value = serde_json::from_str(created.trim()).unwrap();
    // The account is the output key of the Taproot address (D-092): `--producer` accepts both 64 hex digits and an
    // address of this chain's network
    let account = info["account"].as_str().unwrap();
    let address = info["regtest_address"].as_str().unwrap();

    // Node: after printing the funding script and revenue address it exits because bitcoind is unreachable; neither the
    // startup log nor the error message contains the secret key
    let genesis = dir.path().join("genesis.json");
    let file = GenesisFile {
        timestamp: 1_700_000_000,
        btc_anchor: BtcAnchor {
            height: 0,
            hash: [1; 32],
        },
        bits: None,
        block_share_account: None,
        allocations: Vec::new(),
    };
    std::fs::write(&genesis, serde_json::to_string(&file).unwrap()).unwrap();
    let revenue = address.to_string();
    let output = run(Command::new(env!("CARGO_BIN_EXE_ksana-node"))
        .arg("--data-dir")
        .arg(dir.path().join("data"))
        .arg("--genesis")
        .arg(&genesis)
        .args(["--producer", account])
        .args(["--btc-rpc-url", "http://127.0.0.1:9"])
        .arg("--btc-funding-key-file")
        .arg(&key));
    assert!(
        output.contains("locking script of the settlement funding address"),
        "{output}"
    );
    assert!(output.contains("error"), "{output}");
    assert_no_secret(&output, &secret);
    // When the secret key file is malformed, the error message does not echo the file contents either
    let broken = dir.path().join("broken.key");
    std::fs::write(&broken, format!("{secret}00")).unwrap();
    let output = run(Command::new(env!("CARGO_BIN_EXE_ksana-node"))
        .arg("--data-dir")
        .arg(dir.path().join("data2"))
        .arg("--genesis")
        .arg(&genesis)
        .args(["--producer", address])
        .args(["--btc-rpc-url", "http://127.0.0.1:9"])
        .arg("--btc-funding-key-file")
        .arg(&broken)
        .args(["--revenue-address", &revenue]));
    assert!(output.contains("funding secret key"), "{output}");
    assert_no_secret(&output, &secret);
}

/// D-109: on signet a funding key needs the watch-only wallet. The node says so before it reads the key file, the
/// genesis config or bitcoind, so none of them exists here; with the wallet it gets past the check and stops at the
/// missing key file.
#[test]
fn a_funding_key_on_signet_needs_the_wallet() {
    let dir = tempfile::tempdir().unwrap();
    let node = |wallet: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ksana-node"));
        command
            .args(["--chain", "signet"])
            .arg("--data-dir")
            .arg(dir.path().join("data"))
            .arg("--genesis")
            .arg(dir.path().join("genesis.json"))
            .args(["--btc-rpc-url", "http://127.0.0.1:9"])
            .arg("--btc-funding-key-file")
            .arg(dir.path().join("funding.key"));
        if let Some(wallet) = wallet {
            command.args(["--btc-funding-wallet", wallet]);
        }
        run(&mut command)
    };
    let refused = node(None);
    assert!(
        refused.contains("on signet needs --btc-funding-wallet"),
        "{refused}"
    );
    let further = node(Some("ksana-funding"));
    assert!(!further.contains("needs --btc-funding-wallet"), "{further}");
    assert!(further.contains("funding secret key"), "{further}");
}
