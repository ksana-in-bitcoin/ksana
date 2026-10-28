//! M13 end-to-end (D-078, D-085): top-up for KSA -- order (off-chain) → lock (seller, path B) → confirm (buyer,
//! path C) → redeem (the KSA arrives when the claim succeeds), then path B (`tests/integration/run.sh`; needs
//! Node.js ≥ 20 and the built `sdk/ts` and `ksana-cli`, which run.sh builds first).
//!
//! The seller deploys the exchange contract (`contracts/examples/ksa-exchange`). The buyer has no KSA:
//!
//! 1. TypeScript SDK + mock wallet: `regtest.js top-up` -- the seller's mock wallet calls `lock` via path B (locked
//!    for the buyer's account, payment to the seller's P2TR address), the buyer reads the lock and builds a BTC
//!    transaction -- the amount paid to the payment script and a data output carrying the claim op (signed by a
//!    one-time key, nonce 0) -- which the wallet signs and this test broadcasts;
//! 2. `ksana-cli`: the seller runs `exchange lock` (path B, payment to the seller's P2WPKH address), the buyer runs
//!    `top-up --lock`: the same transaction is funded and broadcast by the bitcoind wallet.
//!
//! After the BTC confirmation the claim op is force-included and the KSA is credited to the beneficiary account at the
//! locked price; the buyer then immediately uses KSA via path B, taking effect within seconds. Afterwards: a lock that
//! nobody paid cannot be released by the seller until it has expired and `RELEASE_DELAY` BTC blocks have passed, after
//! which it is released and withdrawn. Finally, prints the BTC byte comparison between "top-up + path B" and the
//! original path A for the same sequence of operations.

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use common::*;
use ksana_execution::builtin::contract_id;
use ksana_integration_tests::Btc;
use ksana_primitives::compact_entries::EntryEncoder;
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, EntryData, FeeMode, Funds, DEVNET, KSA_ASSET_ID};
use ksana_rpc::{KsanaApiClient, OpState, OpStatusJson};
use ksana_test_contracts::KSA_EXCHANGE;
use serde_json::{json, Value};

/// Price: how many KSA base units per sat.
const PRICE: u128 = 100;
/// How many BTC blocks after the lock's expiry the seller can release it (the exchange contract's `RELEASE_DELAY`).
const RELEASE_DELAY: u32 = 6;

/// Runs the built `ksana-cli` (`KSANA_CLI` names the executable, default `target/debug/ksana-cli`) and returns the
/// JSON it prints.
fn run_cli(args: &[String]) -> Value {
    let path = std::env::var_os("KSANA_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/ksana-cli")
        });
    assert!(
        path.exists(),
        "missing {}: build ksana-cli first (cargo build -p ksana-cli; tests/integration/run.sh builds it automatically)",
        path.display()
    );
    let out = Command::new(&path).args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "ksana-cli failed:\n{}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("output of ksana-cli is not JSON ({e}): {stdout}"))
}

/// Mines BTC blocks while waiting, until the claim op is executed.
fn mine_until_executed(btc: &Btc, fast: &Fast, op_id: &[u8; 32]) -> OpStatusJson {
    wait(
        "the claim op to be executed",
        Duration::from_secs(120),
        || {
            mine(btc, 1);
            std::thread::sleep(Duration::from_millis(300));
            op_status(fast, op_id).filter(|s| s.status != OpState::Pending)
        },
    )
}

/// The claim op's receipt: success, returning `(paid_sats, ksa_paid)`.
fn claimed(status: &OpStatusJson) -> (u64, u128) {
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    let receipt = status
        .receipt
        .as_ref()
        .expect("an executed op has a receipt");
    assert!(receipt.success, "{receipt:?}");
    borsh::from_slice(&receipt.output).unwrap()
}

/// Path B: `u` immediately transfers paying the fee in KSA; returns the time from submission to inclusion.
fn pay_in_ksa(fast: &Fast, u: &User, to: [u8; 32]) -> Duration {
    let started = ksana_node::monotonic_now();
    let op = signed(u, 0, FeeMode::Token { amount: 2 }, to, 1_000);
    let status = fast.wait_included(&fast.submit(&op).unwrap());
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    started.elapsed()
}

/// The account's next nonce on the node.
fn nonce_of(fast: &Fast, u: &User) -> u64 {
    fast.rt
        .block_on(fast.client.get_account(hex::encode(u.account)))
        .unwrap()
        .nonce
}

/// The BTC height anchored by the node's latest block.
fn anchor_height(fast: &Fast) -> u32 {
    fast.block(fast.tip_height()).header.btc_anchor.height
}

/// The KSA held by an account (e.g. the exchange contract's account).
fn ksa_of(fast: &Fast, account: [u8; 32]) -> u128 {
    fast.rt
        .block_on(fast.client.get_account(hex::encode(account)))
        .unwrap()
        .balances
        .iter()
        .find(|b| b.asset == KSA_ASSET_ID)
        .map_or(0, |b| b.amount)
}

/// The seller calls a method of the exchange contract (path B) and waits for it to be included.
fn seller_call(
    fast: &Fast,
    seller: &User,
    exchange: [u8; 32],
    method: &str,
    args: Vec<u8>,
    funds: Vec<Funds>,
) -> OpStatusJson {
    let op = signed_action(
        seller,
        nonce_of(fast, seller),
        FeeMode::Token { amount: 1 },
        Action::Call {
            contract: exchange,
            method: method.into(),
            args,
            funds,
        },
        5_000_000,
    );
    fast.wait_included(&fast.submit(&op).unwrap())
}

#[test]
fn top_up_with_the_sdk_and_the_cli_then_pay_fees_in_ksa() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m13");
    let (seller, buyer, cli_buyer, bob) = (
        user("m13/seller"),
        user("m13/buyer"),
        user("m13/cli-buyer"),
        user("m13/bob"),
    );
    let fast = Fast::start(&btc, "m13/top-up", &[(&seller, 100_000_000)], 6);
    let rpc = format!("http://{}", fast.node().rpc_addr());

    // Seller: deploy the exchange contract
    let deploy = signed_action(
        &seller,
        0,
        FeeMode::Token { amount: 1 },
        Action::Deploy {
            code: KSA_EXCHANGE.to_vec(),
            init_args: Vec::new(),
            funds: Vec::new(),
        },
        50_000_000,
    );
    let status = fast.wait_included(&fast.submit(&deploy).unwrap());
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    let exchange = contract_id(&seller.account, 0, &sha256(KSA_EXCHANGE));
    assert_eq!(fast.ksa(&buyer), 0, "the buyer has no KSA yet");
    let expiry = btc_tip(&btc).height + 100;

    // 1. TS SDK + mock wallet: the seller locks 2 000 000 KSA for the buyer (100 per sat, payment to the seller's P2TR
    //    address); the buyer pays 20 000 sats to claim
    let utxo_value = 100_000;
    let utxo = fund_wallets(&btc, vec![wallet_script(&buyer)], utxo_value)[0];
    while fast.node().chain().sync_btc().unwrap() {}
    let out = run_sdk(
        "regtest.js",
        &json!({
            "command": "top-up",
            "rpc": rpc,
            "chain": "devnet",
            "secret_key": hex::encode(buyer.kp.secret_bytes()),
            "utxo": utxo_json(&utxo, utxo_value),
            "exchange": hex::encode(exchange),
            "seller_secret_key": hex::encode(seller.kp.secret_bytes()),
            "ksa": "2000000",
            "price": PRICE.to_string(),
            "expiry": expiry,
            "fee": "1",
            "sats": "20000",
            "fee_rate": 2,
        }),
    );
    assert_eq!(out["lock"], "0", "{out}");
    assert_eq!(out["ksa"], "2000000", "{out}");
    assert_eq!(out["beneficiary"], hex::encode(buyer.account));
    assert_eq!(out["seller"], hex::encode(seller.account));
    assert_eq!(
        out["seller_script"],
        hex::encode(wallet_script(&seller).as_bytes())
    );
    assert_ne!(
        out["one_time_account"],
        hex::encode(buyer.account),
        "the claim op is signed by a one-time key"
    );
    assert_eq!(
        ksa_of(&fast, exchange),
        2_000_000,
        "the locked KSA is in the contract account"
    );
    let raw = out["raw_tx"].as_str().unwrap();
    let txid = btc.call("sendrawtransaction", &[json!(raw)]);
    let op_id = op_id_of(&out);
    let status = mine_until_executed(&btc, &fast, &op_id);
    assert_eq!(claimed(&status), (20_000, 2_000_000));
    assert_eq!(fast.ksa(&buyer), 2_000_000);
    assert_eq!(ksa_of(&fast, exchange), 0, "all locked KSA sold");
    let elapsed = pay_in_ksa(&fast, &buyer, bob.account);
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
    assert_eq!(fast.ksa(&buyer), 2_000_000 - 1_000 - 2);
    let topup_vsize = btc.call("getrawtransaction", &[txid, json!(true)])["vsize"]
        .as_u64()
        .unwrap();

    // 2. ksana-cli: the seller locks 3 000 000 KSA for cli_buyer (payment to the seller's P2WPKH address); the buyer
    //    pays 30 000 sats from the bitcoind wallet
    let key_dir = tempfile::tempdir().unwrap();
    let seller_key = key_dir.path().join("seller.key");
    std::fs::write(&seller_key, hex::encode(seller.kp.secret_bytes())).unwrap();
    let seller_key = seller_key.to_string_lossy().into_owned();
    let out = run_cli(&[
        "--rpc".into(),
        rpc.clone(),
        "exchange".into(),
        "lock".into(),
        "--key".into(),
        seller_key,
        "--fee-token".into(),
        "1".into(),
        "--max-gas".into(),
        "5000000".into(),
        "--exchange".into(),
        hex::encode(exchange),
        "--ksa".into(),
        "3000000".into(),
        "--price".into(),
        PRICE.to_string(),
        "--script".into(),
        hex::encode(p2wpkh_script(&seller).as_bytes()),
        "--beneficiary".into(),
        hex::encode(cli_buyer.account),
        "--expiry".into(),
        expiry.to_string(),
        "--fund".into(),
        "3000000".into(),
        "--wait".into(),
    ]);
    assert_eq!(out["lock"], "1", "{out}");
    assert_eq!(ksa_of(&fast, exchange), 3_000_000);
    let (url, rpc_user, rpc_pass) = btc_env();
    let out = run_cli(&[
        "--rpc".into(),
        rpc.clone(),
        "top-up".into(),
        "--exchange".into(),
        hex::encode(exchange),
        "--lock".into(),
        "1".into(),
        "--beneficiary".into(),
        hex::encode(cli_buyer.account),
        "--sats".into(),
        "30000".into(),
        "--btc-rpc-url".into(),
        url,
        "--btc-rpc-user".into(),
        rpc_user,
        "--btc-rpc-pass".into(),
        rpc_pass,
        "--btc-wallet".into(),
        "ksana-it-m13".into(),
    ]);
    assert_eq!(out["ksa"], "3000000", "{out}");
    assert_eq!(out["sats_for_all"], "30000", "{out}");
    assert_eq!(out["beneficiary"], hex::encode(cli_buyer.account), "{out}");
    let op_id = op_id_of(&out);
    let claim_status = mine_until_executed(&btc, &fast, &op_id);
    assert_eq!(claimed(&claim_status), (30_000, 3_000_000));
    assert!(DEVNET.native_max_gas >= claim_status.receipt.unwrap().gas_used);
    assert_eq!(fast.ksa(&cli_buyer), 3_000_000);
    let elapsed_cli = pay_in_ksa(&fast, &cli_buyer, bob.account);
    assert!(elapsed_cli < Duration::from_secs(30), "{elapsed_cli:?}");
    assert_eq!(fast.ksa(&bob), 2_000);
    // Show the claimed lock
    let shown = run_cli(&[
        "--rpc".into(),
        rpc.clone(),
        "exchange".into(),
        "show".into(),
        "--exchange".into(),
        hex::encode(exchange),
        "--lock".into(),
        "1".into(),
    ]);
    assert_eq!(shown["state"]["claimed"]["paid_sats"], "30000", "{shown}");
    assert_eq!(shown["state"]["claimed"]["ksa_paid"], "3000000", "{shown}");

    // 3. A lock nobody paid: the seller cannot release it before expiry + RELEASE_DELAY, then releases it, and the KSA
    //    returns to the available balance and is withdrawn
    let before = fast.ksa(&seller);
    let short_expiry = anchor_height(&fast) + 2;
    let status = seller_call(
        &fast,
        &seller,
        exchange,
        "lock",
        borsh::to_vec(&(
            1_000_000u128,
            PRICE,
            wallet_script(&bob).to_bytes(),
            bob.account,
            short_expiry,
        ))
        .unwrap(),
        vec![Funds {
            asset: KSA_ASSET_ID,
            amount: 1_000_000,
        }],
    );
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    let lock_id: u64 = borsh::from_slice(&status.receipt.unwrap().output).unwrap();
    assert_eq!(lock_id, 2);
    let status = seller_call(
        &fast,
        &seller,
        exchange,
        "release",
        borsh::to_vec(&lock_id).unwrap(),
        Vec::new(),
    );
    assert_eq!(status.status, OpState::Failed, "{status:?}");
    let error = status.receipt.unwrap().error.unwrap_or_default();
    assert!(error.contains("release delay has not passed"), "{error}");
    // Mine BTC blocks until the node's anchor height reaches expiry + RELEASE_DELAY
    let releasable = short_expiry + RELEASE_DELAY;
    wait(
        "the anchor height to reach the releasable height",
        Duration::from_secs(120),
        || {
            mine(&btc, 1);
            std::thread::sleep(Duration::from_millis(300));
            (anchor_height(&fast) >= releasable).then_some(())
        },
    );
    let status = seller_call(
        &fast,
        &seller,
        exchange,
        "release",
        borsh::to_vec(&lock_id).unwrap(),
        Vec::new(),
    );
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    let released: u128 = borsh::from_slice(&status.receipt.unwrap().output).unwrap();
    assert_eq!(released, 1_000_000);
    let status = seller_call(
        &fast,
        &seller,
        exchange,
        "withdraw",
        borsh::to_vec(&1_000_000u128).unwrap(),
        Vec::new(),
    );
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    assert_eq!(
        fast.ksa(&seller),
        before - 4,
        "apart from the sponsored fees of the four ops (including the failed release), all KSA returns to the seller"
    );
    assert_eq!(ksa_of(&fast, exchange), 0);

    // BTC bytes for the same sequence of operations: one top-up (the user pays the BTC fee) followed by N path-B ops,
    // compared with N ops of the original path A (D-078).
    // Path-B entries enter the settlement payload compacted (D-080): the account and recipient are written in full on
    // first appearance and reference the payload's key dictionary afterwards; here every op is counted as a first
    // appearance; the payload is in the reveal transaction's witness (D-081, 1 byte = 0.25 vB). The original path A
    // (data in OP_RETURN, 1 byte = 1 vB), on top of the Borsh entry: the fee mode is 28 bytes larger (funding outpoint
    // and contribution instead of the token amount),
    // the entry carries a `Contribution` (nSequence 4, P2TR witness 4 + 4 + 65, paired output 8 + 4 + 34, 123 bytes
    // in total), and the settlement transaction has one more P2TR input (41 + 67 / 4 ≈ 58 vB) and paired output
    // (43 vB).
    let op = signed(&buyer, 1, FeeMode::Token { amount: 2 }, bob.account, 1_000);
    let entry = EntryData::Sponsored { op };
    let path_b_borsh = borsh::to_vec(&entry).unwrap().len() as u64;
    let mut encoder = EntryEncoder::new(DEVNET.chain_id);
    let path_b = encoder.encode(&entry).unwrap().len() as u64;
    let path_b_repeat = encoder.encoded_len(&entry).unwrap() as u64;
    let claim_entry = EntryEncoder::new(DEVNET.chain_id)
        .encode(&EntryData::Forced { btc_txid: [0; 32] })
        .unwrap()
        .len() as u64;
    let path_a = path_b_borsh + 28 + 123 + 58 + 43;
    let n = 60;
    let top_up_total = topup_vsize + (claim_entry + n * path_b).div_ceil(4);
    let path_a_total = n * path_a;
    eprintln!(
        "BTC bytes of top-up + path B vs the original path A ({n} KSA transfers, D-078, D-080, D-081):\n\
         \x20 top-up transaction {topup_vsize} vB (payment and the data output carrying the claim op), claim entry in the settlement payload {claim_entry} bytes\n\
         \x20 path B {path_b} bytes per op, about {:.0} vB (compact entry in the witness; an account repeated within the same payload {path_b_repeat} bytes, uncompressed \
         {path_b_borsh} bytes); original path A about {path_a} vB per op\n\
         \x20 total: top-up + path B {top_up_total} vB, original path A {path_a_total} vB (the former is {}% of the latter)\n\
         \x20 submission to effect: {elapsed:?} after the SDK top-up, {elapsed_cli:?} after the CLI top-up",
        path_b as f64 / 4.0,
        top_up_total * 100 / path_a_total
    );
    assert!(top_up_total < path_a_total);
}
