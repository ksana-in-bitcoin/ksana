//! M48 on the devnet's regtest bitcoind (D-131, `tests/integration/run.sh`): a lone producer's settlement whose P2A
//! output another party's child took first goes out again in a new pair of the producer's, against Bitcoin Core's own
//! mempool. The node tests (`crates/ksana-node/tests/adversarial_liveness.rs`) cover the same on the mock chain, which
//! has no RBF and refuses any conflict; here bitcoind itself refuses the producer's CPFP child (BIP125 rule 3) and
//! answers `gettxout` with the mempool.
//!
//! Only the test itself mines BTC blocks. Run in order with `--test-threads=1`.

mod common;

use std::str::FromStr;
use std::time::Duration;

use bitcoin::consensus::encode::{deserialize, deserialize_hex};
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::script::{Builder, PushBytesBuf};
use bitcoin::{
    absolute, transaction, Amount, OutPoint as BtcOutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxOut, Txid, Witness,
};
use common::*;
use ksana_btc::payload::envelope_payload;
use ksana_btc::settlement::{cpfp_child, p2a_script, ProducerUtxo};
use ksana_integration_tests::{data_output, Btc};
use ksana_node::rescue::BUMP_AFTER;
use ksana_primitives::{FeeMode, DEVNET};
use ksana_rpc::SettlementState;
use serde_json::{json, Value};

fn in_mempool(btc: &Btc, txid: &Txid) -> bool {
    btc.try_call("getmempoolentry", &[json!(txid.to_string())])
        .is_ok()
}

/// The transactions in bitcoind's mempool.
fn mempool_txs(btc: &Btc) -> Vec<Transaction> {
    let ids = btc.call("getrawmempool", &[]);
    ids.as_array()
        .unwrap()
        .iter()
        .filter_map(|id| {
            let hex = btc
                .try_call("getrawtransaction", std::slice::from_ref(id))
                .ok()?;
            deserialize_hex(hex.as_str()?).ok()
        })
        .collect()
}

/// The transactions in bitcoind's mempool whose envelope carries `payload`.
fn carrying(btc: &Btc, payload: &[u8]) -> Vec<Transaction> {
    mempool_txs(btc)
        .into_iter()
        .filter(|tx| envelope_payload(&DEVNET, tx).as_deref() == Some(payload))
        .collect()
}

/// The fee (sats) of a transaction in bitcoind's mempool.
fn mempool_fee(btc: &Btc, txid: &Txid) -> u64 {
    let entry = btc.call("getmempoolentry", &[json!(txid.to_string())]);
    Amount::from_btc(entry["fees"]["base"].as_f64().unwrap())
        .unwrap()
        .to_sat()
}

/// The fees (sats) and virtual size (vB) of a commit / reveal pair together, as the node counts them.
fn pair_fee(btc: &Btc, commit: &Transaction, reveal: &Transaction) -> (u64, u64) {
    let fee = mempool_fee(btc, &commit.compute_txid()) + mempool_fee(btc, &reveal.compute_txid());
    let vsize = commit.weight().to_vbytes_ceil() + reveal.weight().to_vbytes_ceil();
    (fee, vsize)
}

/// Mines one BTC block holding `txids` from the mempool and nothing else (`generateblock`): the other transactions
/// keep waiting there.
fn mine_only(btc: &Btc, txids: &[Txid]) {
    let txids: Vec<String> = txids.iter().map(Txid::to_string).collect();
    btc.call("generateblock", &[json!(btc.new_address()), json!(txids)]);
}

/// The confirmed UTXOs paying the producer's funding script.
fn funding_utxos(btc: &Btc, fast: &Fast) -> Vec<ProducerUtxo> {
    let descriptor = format!("raw({})", fast.funding.script.to_hex_string());
    let scan = btc.call("scantxoutset", &[json!("start"), json!([descriptor])]);
    scan["unspents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| ProducerUtxo {
            outpoint: BtcOutPoint::new(
                Txid::from_str(u["txid"].as_str().unwrap()).unwrap(),
                u32::try_from(u["vout"].as_u64().unwrap()).unwrap(),
            ),
            txout: TxOut {
                value: Amount::from_btc(u["amount"].as_f64().unwrap()).unwrap(),
                script_pubkey: fast.funding.script.clone(),
            },
        })
        .collect()
}

/// Takes the P2A output of `reveal` first, as anyone may (D-131): a child of about 50 kvB at the relay floor of
/// 1 sat/vB, funded by the wallet. A CPFP child of the producer's would have to pay more than its whole fee to replace
/// it (BIP125 rule 3).
fn pin(btc: &Btc, reveal: &Transaction) -> Transaction {
    let unspent = btc.call("listunspent", &[json!(1)]);
    let coin = unspent
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["amount"].as_f64().unwrap() >= 0.01)
        .expect("a wallet UTXO of at least 0.01 BTC");
    let point = BtcOutPoint::new(
        Txid::from_str(coin["txid"].as_str().unwrap()).unwrap(),
        u32::try_from(coin["vout"].as_u64().unwrap()).unwrap(),
    );
    let value = Amount::from_btc(coin["amount"].as_f64().unwrap()).unwrap();
    let info = btc.call("getaddressinfo", &[json!(btc.new_address())]);
    let change =
        ScriptBuf::from_bytes(hex::decode(info["scriptPubKey"].as_str().unwrap()).unwrap());
    let mut data = Builder::new().push_opcode(OP_RETURN);
    for _ in 0..96 {
        data = data.push_slice(PushBytesBuf::try_from(vec![0xa5; 520]).unwrap());
    }
    let input = |previous_output| TxIn {
        previous_output,
        script_sig: ScriptBuf::new(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        witness: Witness::new(),
    };
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: vec![
            input(BtcOutPoint::new(reveal.compute_txid(), 0)),
            input(point),
        ],
        output: vec![
            data_output(data.into_script()),
            TxOut {
                value: Amount::ZERO,
                script_pubkey: change,
            },
        ],
    };
    // 1 sat/vB, the wallet input's witness (at most 28 vB) included
    let fee = Amount::from_sat(tx.weight().to_vbytes_ceil() + 28);
    tx.output[1].value = value + reveal.output[0].value - fee;
    let tx = btc.sign_with_wallet(&tx);
    btc.send(&tx);
    tx
}

/// D-131 against a real mempool: another party's large child at the relay floor takes a lone producer's settlement's
/// P2A output first. bitcoind's `gettxout` with the mempool then has nothing for the output, and it refuses a CPFP
/// child of the producer's at the bump's rate for paying less than the child it would replace. After `BUMP_AFTER` BTC
/// blocks without the pair, the producer's bump is refused that way, and the settlement's payload goes out again, once,
/// in a new pair at the bump's rate that spends nothing of the old pair's. The new pair lands on its own and the
/// block's data is confirmed through it, while the pinned pair and the pinning child still wait; nothing more is spent
/// on the pinned pair afterwards.
#[test]
fn a_settlement_whose_p2a_output_another_party_holds_goes_out_again() {
    let btc = Btc::from_env().funded_wallet("ksana-it-m48");
    let (alice, bob) = (user("m48/pinned/alice"), user("m48/pinned/bob"));
    let fast = Fast::start(&btc, "m48/pinned", &[(&alice, 1_000)], 6);
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let block = fast
        .wait_included(&fast.submit(&op).unwrap())
        .block
        .unwrap();
    let s = fast.wait_settlement(block.height, SettlementState::Broadcast);
    let txs = s.txs.expect("the block carries settlement transactions");
    let commit: Transaction = deserialize(&txs.commit).unwrap();
    let reveal: Transaction = deserialize(&txs.reveal).unwrap();
    let payload = envelope_payload(&DEVNET, &reveal).expect("the reveal carries the payload");
    assert_eq!(reveal.output[0].script_pubkey, p2a_script());
    let anchor = BtcOutPoint::new(reveal.compute_txid(), 0);
    let (fee, vsize) = pair_fee(&btc, &commit, &reveal);
    let own = fee / vsize;

    let pinning = pin(&btc, &reveal);
    assert!(in_mempool(&btc, &pinning.compute_txid()));
    let pinning_fee = mempool_fee(&btc, &pinning.compute_txid());
    // What the node reads (D-131): bitcoind's gettxout with the mempool has nothing for the P2A output ...
    let held = btc.call(
        "gettxout",
        &[
            json!(anchor.txid.to_string()),
            json!(anchor.vout),
            json!(true),
        ],
    );
    assert_eq!(held, Value::Null, "the pinning child spends the P2A output");
    // ... and testmempoolaccept refuses a child like the producer's first bump, at twice the pair's rate (D-117)
    let spent: Vec<BtcOutPoint> = commit.input.iter().map(|i| i.previous_output).collect();
    let utxo = funding_utxos(&btc, &fast)
        .into_iter()
        .find(|u| !spent.contains(&u.outpoint))
        .expect("a funding UTXO the pair does not spend");
    let (child, child_fee) = cpfp_child(
        &DEVNET,
        &reveal,
        fee,
        vsize,
        &utxo,
        own * 2,
        0,
        &fast.funding.keypair,
    )
    .unwrap()
    .expect("the funding UTXO pays for the child");
    assert!(child_fee < pinning_fee);
    let refused = btc
        .test_accept(&child)
        .expect_err("a child paying less than the pinning one is refused");
    assert!(
        refused.contains("insufficient fee"),
        "refused for another reason: {refused}"
    );
    println!(
        "pair {} at {own} sat/vB ({fee} sats over {vsize} vB); pinning child {} pays {pinning_fee} sats over {} vB; \
         gettxout with the mempool: {held}; a child of the producer's paying {child_fee} sats: {refused}",
        reveal.compute_txid(),
        pinning.compute_txid(),
        pinning.weight().to_vbytes_ceil(),
    );

    // BTC blocks without the pair: after BUMP_AFTER of them the producer's bump is refused, and the payload goes out
    // again in a new pair of the producer's
    let mut mined = 0;
    let again = loop {
        assert!(mined < BUMP_AFTER + 4, "the payload did not go out again");
        mine_only(&btc, &[]);
        mined += 1;
        let found = (0..60).find_map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            carrying(&btc, &payload)
                .into_iter()
                .find(|tx| tx.compute_txid() != reveal.compute_txid())
        });
        if let Some(tx) = found {
            break tx;
        }
    };
    assert!(mined >= BUMP_AFTER, "not before BUMP_AFTER BTC blocks");
    let again_commit_txid = again.input[0].previous_output.txid;
    let again_commit: Transaction = deserialize_hex(
        btc.call("getrawtransaction", &[json!(again_commit_txid.to_string())])
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let old = [commit.compute_txid(), reveal.compute_txid()];
    assert!(
        again_commit
            .input
            .iter()
            .all(|i| !old.contains(&i.previous_output.txid) && !spent.contains(&i.previous_output)),
        "the new pair spends nothing of the old pair's"
    );
    let (again_fee, again_vsize) = pair_fee(&btc, &again_commit, &again);
    assert!(
        again_fee / again_vsize >= own * 2,
        "at the bump's rate: {again_fee} sats over {again_vsize} vB"
    );
    println!(
        "after {mined} BTC blocks: sent again as {} ({again_fee} sats over {again_vsize} vB)",
        again.compute_txid()
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(carrying(&btc, &payload).len(), 2, "only once");

    // The new pair lands on its own; the block's data is confirmed through it while the pinned pair waits
    mine_only(&btc, &[again_commit_txid, again.compute_txid()]);
    let s = fast.wait_settlement(block.height, SettlementState::Confirmed);
    assert_eq!(
        s.confirmed.unwrap().txid,
        again.compute_txid().to_byte_array()
    );
    assert!(in_mempool(&btc, &reveal.compute_txid()) && in_mempool(&btc, &pinning.compute_txid()));

    // Nothing more is spent on the pinned pair, and its payload does not go out again
    for _ in 0..=BUMP_AFTER {
        mine_only(&btc, &[]);
        std::thread::sleep(Duration::from_secs(1));
    }
    let children: Vec<Txid> = mempool_txs(&btc)
        .iter()
        .filter(|tx| tx.input.iter().any(|i| i.previous_output == anchor))
        .map(Transaction::compute_txid)
        .collect();
    assert_eq!(children, [pinning.compute_txid()], "not bumped");
    assert_eq!(carrying(&btc, &payload).len(), 1, "not sent again");

    // Leave the mempool empty for the next tests
    mine(&btc, 1);
}
