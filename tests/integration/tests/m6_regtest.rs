//! M6 end-to-end: the TypeScript SDK and the mock wallet complete path B and path C on regtest, and the wallet demo
//! page completes path B in a browser (`tests/integration/run.sh`; needs Node.js ≥ 20, run.sh builds `sdk/ts` first).
//! Path A has been removed (D-078); the former path-A flow is now path B.
//!
//! The mock wallet implements the same injected interface as Unisat (`sdk/ts/src/wallet/mock.ts`) and is driven by
//! the SDK's Unisat adapter; two Taproot accounts (since D-092 an account is a Taproot address, and wallets can only
//! use Taproot addresses). The SDK-side flows are in `sdk/ts/test/e2e/`:
//!
//! - `regtest.ts` path B: query the nonce, build a VmOp with the fee in KSA, the wallet signs with BIP322 simple,
//!   submit via `ksana_submitOp` and wait for fast_confirmed;
//! - `regtest.ts` path C: build a native transaction with a KSN1 data output (the VmOp is likewise BIP322-signed by
//!   the wallet), the wallet signs and outputs the raw transaction, which this test broadcasts;
//! - `demo-page.ts`: open `sdk/ts/examples/wallet-demo` in Chromium (with the mock wallet injected), click connect,
//!   estimate the fee, sign and submit; the page calls the node RPC via CORS. Needs a Chromium that playwright-core
//!   can launch (`npx playwright-core install --only-shell chromium`, or name the executable with `KSANA_BROWSER`);
//!   set `KSANA_SKIP_BROWSER=1` to skip, `KSANA_DEMO_SCREENSHOT=<path>` to save a screenshot once done.
//!
//! This test prepares the chain and funds, and checks independently on the node: the signature scheme is
//! `Bip322Simple`, the fee is deducted from the account per the VmOp, the ops become btc_final once the settlement is
//! confirmed deep enough; the native transactions are force-included and execute successfully.

mod common;

use std::time::Duration;

use bitcoin::consensus::encode::deserialize;
use bitcoin::hashes::Hash;
use bitcoin::Transaction;
use common::*;
use ksana_integration_tests::Btc;
use ksana_primitives::{Entry, FeeMode, SigScheme};
use ksana_rpc::{Cors, OpState};
use serde_json::json;

/// An included path-B op: checks that it is `u`'s, BIP322-signed by the wallet, with a fee of `fee` KSA; returns its
/// height.
fn check_sponsored(fast: &Fast, u: &User, kind: &str, op_id: &[u8; 32], fee: u128) -> u64 {
    let status = fast.status(op_id);
    assert_eq!(status.status, OpState::FastConfirmed, "{kind}");
    let height = status.block.unwrap().height;
    let block = fast.block(height);
    let op = block
        .entries
        .iter()
        .find_map(|e| match e {
            Entry::Sponsored { op } if op.op_id().unwrap() == *op_id => Some(op),
            _ => None,
        })
        .expect("path-B entry");
    assert_eq!(op.account, u.account, "{kind}");
    assert!(
        matches!(&op.sig, SigScheme::Bip322Simple(_)),
        "{kind}: the VmOp is BIP322-signed by the wallet"
    );
    assert_eq!(op.fee, FeeMode::Token { amount: fee }, "{kind}");
    height
}

#[test]
fn sdk_and_mock_wallet_complete_paths_b_and_c() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m6");
    let (alice, carol, bob) = (user("m6/alice"), user("m6/carol"), user("m6/bob"));
    let fast = Fast::start(&btc, "m6/sdk", &[(&alice, 1_000), (&carol, 1_000)], 6);
    let rpc = format!("http://{}", fast.node().rpc_addr());
    let wallets = [(&alice, "alice"), (&carol, "carol")];

    // Path B: submitted by the SDK + mock wallet; the signature scheme and fee are checked on the node
    let mut included = Vec::new();
    for (u, kind) in &wallets {
        let out = run_sdk(
            "regtest.js",
            &json!({
                "command": "path-b",
                "rpc": rpc,
                "chain": "devnet",
                "secret_key": hex::encode(u.kp.secret_bytes()),
                "to": hex::encode(bob.account),
                "amount": "7",
                "fee": "2",
            }),
        );
        assert_eq!(out["status"], "fast_confirmed", "{kind}: {out}");
        assert_eq!(out["nonce"], "0", "{kind}");
        let op_id = op_id_of(&out);
        let height = check_sponsored(&fast, u, kind, &op_id, 2);
        assert_eq!(out["block"]["height"], height.to_string(), "{kind}");
        assert_eq!(
            fast.ksa(u),
            1_000 - 7 - 2,
            "{kind}: the amount and fee are deducted from the account"
        );
        included.push(op_id);
    }
    wait(
        "the path-B ops to become btc_final",
        Duration::from_secs(120),
        || {
            mine(&btc, 1);
            std::thread::sleep(Duration::from_millis(300));
            included
                .iter()
                .all(|id| fast.status(id).status == OpState::BtcFinal)
                .then_some(())
        },
    );
    assert_eq!(fast.ksa(&bob), 7 * 2);

    // Path C: the SDK builds the native transaction, the wallet signs, this test broadcasts; force-included after the
    // BTC confirmation
    let value = 60_000;
    let utxos = fund_wallets(
        &btc,
        wallets.iter().map(|(u, _)| wallet_script(u)).collect(),
        value,
    );
    while fast.node().chain().sync_btc().unwrap() {}
    for ((u, kind), point) in wallets.iter().zip(&utxos) {
        let out = run_sdk(
            "regtest.js",
            &json!({
                "command": "path-c",
                "rpc": rpc,
                "chain": "devnet",
                "secret_key": hex::encode(u.kp.secret_bytes()),
                "utxo": utxo_json(point, value),
                "fee_rate": 2,
                "to": hex::encode(bob.account),
                "amount": "5",
            }),
        );
        let op_id = op_id_of(&out);
        let raw = out["raw_tx"].as_str().unwrap();
        let tx: Transaction = deserialize(&hex::decode(raw).unwrap()).unwrap();
        assert_eq!(tx.input[0].previous_output, *point, "{kind}");
        btc.call("sendrawtransaction", &[json!(raw)]);
        let status = wait(
            "the native transaction to be force-included",
            Duration::from_secs(120),
            || {
                mine(&btc, 1);
                std::thread::sleep(Duration::from_millis(300));
                op_status(&fast, &op_id).filter(|s| s.status != OpState::Pending)
            },
        );
        assert_eq!(status.status, OpState::FastConfirmed, "{kind}");
        let block = fast.block(status.block.unwrap().height);
        assert!(
            block.entries.iter().any(|e| matches!(
                e,
                Entry::Forced { btc_txid, op }
                    if *btc_txid == tx.compute_txid().to_byte_array()
                        && op.op_id().unwrap() == op_id
                        && op.account == u.account
                        && op.fee == FeeMode::BtcNative
                        && matches!(op.sig, SigScheme::Bip322Simple(_))
            )),
            "{kind}: the force-included native transaction"
        );
    }
    assert_eq!(fast.ksa(&bob), 7 * 2 + 5 * 2);
}

/// The wallet demo page (`sdk/ts/examples/wallet-demo`) completes path B in Chromium with the mock wallet injected:
/// the page fills in the fee per `ksana_estimateFee`, the wallet signs with BIP322, and the page submits by calling
/// the node RPC via CORS.
#[test]
fn wallet_demo_page_completes_path_b_in_a_browser() {
    if std::env::var_os("KSANA_SKIP_BROWSER").is_some() {
        eprintln!("skipped: KSANA_SKIP_BROWSER is set");
        return;
    }
    let btc = Btc::from_env().funded_wallet("ksana-it-m6");
    let (alice, bob) = (user("m6/page"), user("m6/page-bob"));
    let fast = Fast::start_with_cors(&btc, "m6/page", &[(&alice, 1_000)], 6, Cors::Any);

    let mut input = json!({
        "rpc": format!("http://{}", fast.node().rpc_addr()),
        "secret_key": hex::encode(alice.kp.secret_bytes()),
        "to": hex::encode(bob.account),
        "amount": "3",
    });
    if let Ok(browser) = std::env::var("KSANA_BROWSER") {
        input["browser"] = json!(browser);
    }
    // Full-page screenshot once done (for checking the page manually)
    if let Ok(path) = std::env::var("KSANA_DEMO_SCREENSHOT") {
        input["screenshot"] = json!(path);
    }
    let out = run_sdk("demo-page.js", &input);
    assert_eq!(
        out["account"],
        hex::encode(alice.account),
        "the account shown by the page"
    );
    let op_id = op_id_of(&out);
    let fee: u128 = out["fee"].as_str().unwrap().parse().unwrap();
    assert!(fee > 0, "the suggested fee is used");
    check_sponsored(&fast, &alice, "page", &op_id, fee);
    assert_eq!(fast.ksa(&bob), 3);
    assert_eq!(fast.ksa(&alice), 1_000 - 3 - fee);
}
