//! M3: the behavior of `ksana_btc::client::RpcClient` on a real regtest bitcoind (`tests/integration/run.sh`).
//!
//! Covers every method of `BtcClient`: tip and block queries (`-8` / `-5` → `None`, internal byte order),
//! transactions in the mempool and confirmed, UTXOs with / without the mempool, UTXO scan of a fresh address
//! (including coinbase), testmempoolaccept rejection reasons, the various results of sendrawtransaction (already in
//! the mempool, already confirmed, a confirmed transaction with all outputs spent, rejected, undecodable), blocks
//! leaving the best chain and the transactions in them, local faults.
//! The connection parameters are the same as `ksana_integration_tests::Btc` (`KSANA_BTC_RPC_URL` /
//! `KSANA_BTC_RPC_USER` / `KSANA_BTC_RPC_PASS`).

use std::panic::AssertUnwindSafe;
use std::str::FromStr;

use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::hashes::Hash;
use bitcoin::{
    absolute, transaction, Address, Amount, BlockHash, Network, OutPoint, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Txid, Witness,
};
use ksana_btc::client::{BtcClient, BtcError, RpcClient, ScannedUtxo, TxInfo, UtxoInfo};
use ksana_integration_tests::Btc;
use serde_json::json;

const WALLET: &str = "m3-client";

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn rpc_url() -> String {
    env("KSANA_BTC_RPC_URL", "http://127.0.0.1:18443")
}

fn client() -> RpcClient {
    RpcClient::new(
        &rpc_url(),
        &env("KSANA_BTC_RPC_USER", "ksana"),
        &env("KSANA_BTC_RPC_PASS", "ksana"),
    )
    .unwrap()
}

/// A test wallet with a spendable balance; mines one block first to confirm the transactions left in the mempool, so
/// that the wallet spends only confirmed outputs.
fn wallet() -> Btc {
    let wallet = Btc::from_env().funded_wallet(WALLET);
    let addr = wallet.new_address();
    wallet.mine(1, &addr);
    wallet
}

/// A new wallet address and its locking script.
fn fresh(wallet: &Btc) -> (String, ScriptBuf) {
    let addr = wallet.new_address();
    let script = Address::from_str(&addr)
        .unwrap()
        .require_network(Network::Regtest)
        .unwrap()
        .script_pubkey();
    (addr, script)
}

/// RPC hex hash (display byte order) → internal byte order (D-001).
fn internal(hex: &str) -> [u8; 32] {
    BlockHash::from_str(hex).unwrap().to_byte_array()
}

fn txid(tx: &Transaction) -> [u8; 32] {
    tx.compute_txid().to_byte_array()
}

fn out(tx: &Transaction, vout: u32) -> OutPoint {
    OutPoint {
        txid: tx.compute_txid(),
        vout,
    }
}

/// The index of the output of `tx` whose locking script is `script` (fundrawtransaction inserts the change at a
/// random position).
fn vout_of(tx: &Transaction, script: &ScriptBuf) -> u32 {
    let vout = tx
        .output
        .iter()
        .position(|o| &o.script_pubkey == script)
        .unwrap();
    u32::try_from(vout).unwrap()
}

fn pay(value: u64, script: &ScriptBuf) -> TxOut {
    TxOut {
        value: Amount::from_sat(value),
        script_pubkey: script.clone(),
    }
}

/// An unsigned v2 transaction spending `inputs` with a single output.
fn spend(inputs: &[OutPoint], value: u64, script: &ScriptBuf) -> Transaction {
    Transaction {
        version: transaction::Version::TWO,
        lock_time: absolute::LockTime::ZERO,
        input: inputs
            .iter()
            .map(|op| TxIn {
                previous_output: *op,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            })
            .collect(),
        output: vec![pay(value, script)],
    }
}

#[test]
fn chain_queries_match_bitcoind() {
    let btc = Btc::from_env();
    let _wallet = wallet();
    let c = client();

    let info = btc.call("getblockchaininfo", &[]);
    let (height, hash) = c.tip().unwrap();
    assert_eq!(u64::from(height), info["blocks"].as_u64().unwrap());
    assert_eq!(hash, internal(info["bestblockhash"].as_str().unwrap()));
    assert_eq!(c.block_hash(height).unwrap(), Some(hash));
    let genesis = bitcoin::constants::genesis_block(Network::Regtest);
    assert_eq!(
        c.block_hash(0).unwrap(),
        Some(genesis.block_hash().to_byte_array())
    );
    // Above the tip: bitcoind returns -8.
    assert_eq!(c.block_hash(height + 1).unwrap(), None);
    assert_eq!(c.block_hash(u32::MAX).unwrap(), None);

    let block = c.block(&hash).unwrap().unwrap();
    assert_eq!(block.block_hash().to_byte_array(), hash);
    assert!(block.check_merkle_root());
    let raw = btc.call(
        "getblock",
        &[json!(block.block_hash().to_string()), json!(0)],
    );
    assert_eq!(serialize_hex(&block), raw.as_str().unwrap());
    assert_eq!(
        c.block(&genesis.block_hash().to_byte_array()).unwrap(),
        Some(genesis)
    );

    let header = c.header(&hash).unwrap().unwrap();
    assert_eq!(
        (header.height, header.in_best_chain, header.header),
        (height, true, block.header)
    );
    let parent = c
        .header(&block.header.prev_blockhash.to_byte_array())
        .unwrap()
        .unwrap();
    assert_eq!((parent.height, parent.in_best_chain), (height - 1, true));

    // Unknown block: bitcoind returns -5. The reversed hash is unknown as well (the byte order must not be reversed).
    let mut reversed = hash;
    reversed.reverse();
    assert_eq!(c.block(&reversed).unwrap(), None);
    assert_eq!(c.header(&reversed).unwrap(), None);
    assert_eq!(c.block(&[0xaa; 32]).unwrap(), None);
    assert_eq!(c.header(&[0xaa; 32]).unwrap(), None);
    assert_eq!(c.transaction(&[0xaa; 32]).unwrap(), None);
    assert!(!c.in_mempool(&[0xaa; 32]).unwrap());
    let nowhere = OutPoint {
        txid: Txid::from_byte_array([0xaa; 32]),
        vout: 0,
    };
    assert_eq!(c.utxo(&nowhere, true).unwrap(), None);
}

#[test]
fn transactions_utxos_and_broadcast() {
    let wallet = wallet();
    let c = client();
    let (addr, _) = fresh(&wallet);
    let (_, s1) = fresh(&wallet);
    let (_, s2) = fresh(&wallet);
    let (_, dest) = fresh(&wallet);
    let tx = wallet.fund_and_sign(vec![pay(100_000, &s1), pay(200_000, &s2)]);
    let id = txid(&tx);

    // The same transaction without witnesses (same txid): script verification fails.
    let mut unsigned = tx.clone();
    for input in &mut unsigned.input {
        input.witness = Witness::new();
    }
    let reason = c.test_accept(&[&unsigned]).unwrap().unwrap_err();
    assert!(
        reason.starts_with("mandatory-script-verify-flag-failed"),
        "{reason}"
    );
    let reason = c.broadcast(&unsigned).unwrap().unwrap_err();
    assert!(
        reason.starts_with("mandatory-script-verify-flag-failed"),
        "{reason}"
    );

    // Before and after broadcasting.
    assert_eq!(c.test_accept(&[&tx]).unwrap(), Ok(()));
    assert!(!c.in_mempool(&id).unwrap());
    assert_eq!(c.transaction(&id).unwrap(), None);
    assert_eq!(c.broadcast(&tx).unwrap(), Ok(()));
    assert_eq!(c.broadcast(&tx).unwrap(), Ok(()), "already in the mempool");
    assert!(c.in_mempool(&id).unwrap());
    assert_eq!(
        c.transaction(&id).unwrap(),
        Some(TxInfo {
            tx: tx.clone(),
            block: None
        })
    );
    assert_eq!(
        c.test_accept(&[&tx]).unwrap(),
        Err("txn-already-in-mempool".into())
    );

    // New outputs in the mempool are visible only when the mempool is included; confirmed outputs spent by mempool
    // transactions count as spent only when the mempool is included.
    let v1 = vout_of(&tx, &s1);
    let op1 = out(&tx, v1);
    assert_eq!(c.utxo(&op1, false).unwrap(), None);
    assert_eq!(
        c.utxo(&op1, true).unwrap(),
        Some(UtxoInfo {
            txout: pay(100_000, &s1),
            confirmations: 0,
            coinbase: false,
        })
    );
    let input = tx.input[0].previous_output;
    let spent = c.utxo(&input, false).unwrap().unwrap();
    assert!(spent.confirmations >= 1);
    let funding = c.transaction(&input.txid.to_byte_array()).unwrap().unwrap();
    assert!(funding.block.is_some());
    assert_eq!(spent.txout, funding.tx.output[input.vout as usize]);
    assert_eq!(c.utxo(&input, true).unwrap(), None);

    // After confirmation.
    let mined = wallet.mine(1, &addr);
    let block = internal(&mined[0]);
    assert_eq!(c.tip().unwrap().1, block);
    assert!(!c.in_mempool(&id).unwrap());
    assert_eq!(
        c.transaction(&id).unwrap(),
        Some(TxInfo {
            tx: tx.clone(),
            block: Some(block)
        })
    );
    assert_eq!(
        c.utxo(&op1, false).unwrap(),
        Some(UtxoInfo {
            txout: pay(100_000, &s1),
            confirmations: 1,
            coinbase: false,
        })
    );
    assert_eq!(c.utxo(&input, true).unwrap(), None);
    // Confirmed and still has unspent outputs: bitcoind returns -27.
    assert_eq!(c.broadcast(&tx).unwrap(), Ok(()));
    assert_eq!(
        c.test_accept(&[&tx]).unwrap(),
        Err("txn-already-known".into())
    );

    // Broadcasting tx again after spending all its outputs: bitcoind returns -25 (inputs spent), but the transaction
    // is confirmed, so it still counts as success.
    let all: Vec<OutPoint> = (0..tx.output.len())
        .map(|v| out(&tx, u32::try_from(v).unwrap()))
        .collect();
    let total: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    let sweep = spend(&all, total - 2_000, &dest);
    let signed = wallet.call(
        "signrawtransactionwithwallet",
        &[json!(serialize_hex(&sweep))],
    );
    assert_eq!(signed["complete"], true, "{signed}");
    let sweep: Transaction = deserialize_hex(signed["hex"].as_str().unwrap()).unwrap();
    assert_eq!(c.broadcast(&sweep).unwrap(), Ok(()));
    wallet.mine(1, &addr);
    for op in &all {
        assert_eq!(c.utxo(op, true).unwrap(), None);
    }
    assert_eq!(c.broadcast(&tx).unwrap(), Ok(()));
    assert!(!c.in_mempool(&id).unwrap());

    // Rejected transactions.
    let missing = spend(
        &[OutPoint {
            txid: Txid::from_byte_array([0xaa; 32]),
            vout: 0,
        }],
        1_000,
        &dest,
    );
    assert_eq!(
        c.test_accept(&[&missing]).unwrap(),
        Err("missing-inputs".into())
    );
    assert_eq!(
        c.broadcast(&missing).unwrap(),
        Err("bad-txns-inputs-missingorspent".into())
    );
    let double = spend(&[all[0]], 1_000, &dest);
    assert_eq!(
        c.broadcast(&double).unwrap(),
        Err("bad-txns-inputs-missingorspent".into())
    );
    // A zero-input transaction is serialized in the segwit format, which bitcoind cannot decode (-22); also returned
    // as a rejection.
    let empty = spend(&[], 1_000, &dest);
    assert!(matches!(c.test_accept(&[&empty]), Ok(Err(_))));
    assert!(matches!(c.broadcast(&empty), Ok(Err(_))));
}

#[test]
fn scan_utxos_of_a_fresh_address() {
    let wallet = wallet();
    let c = client();
    let (addr, s) = fresh(&wallet);
    let (other, _) = fresh(&wallet);
    assert!(c.scan_utxos(&s).unwrap().is_empty());

    let tx = wallet.fund_and_sign(vec![pay(30_000, &s), pay(40_000, &s)]);
    assert_eq!(c.broadcast(&tx).unwrap(), Ok(()));
    assert!(
        c.scan_utxos(&s).unwrap().is_empty(),
        "scantxoutset ignores the mempool"
    );
    let (height, _) = c.tip().unwrap();
    wallet.mine(1, &other);
    let mut expected: Vec<ScannedUtxo> = (0u32..)
        .zip(&tx.output)
        .filter(|(_, o)| o.script_pubkey == s)
        .map(|(vout, o)| ScannedUtxo {
            outpoint: out(&tx, vout),
            txout: o.clone(),
            height: height + 1,
            coinbase: false,
        })
        .collect();
    expected.sort_by_key(|u| u.outpoint);
    assert_eq!(expected.len(), 2);
    assert_eq!(c.scan_utxos(&s).unwrap(), expected);

    // Coinbase output.
    let hash = internal(&wallet.mine(1, &addr)[0]);
    let block = c.block(&hash).unwrap().unwrap();
    let coinbase = &block.txdata[0];
    let scanned = c.scan_utxos(&s).unwrap();
    assert_eq!(scanned.len(), 3);
    assert_eq!(scanned[..2], expected[..]);
    assert_eq!(
        scanned[2],
        ScannedUtxo {
            outpoint: out(coinbase, 0),
            txout: coinbase.output[0].clone(),
            height: height + 2,
            coinbase: true,
        }
    );
    let info = c.utxo(&out(coinbase, 0), false).unwrap().unwrap();
    assert!(info.coinbase);
    assert_eq!(info.confirmations, 1);
    assert_eq!(
        c.transaction(&txid(coinbase)).unwrap().unwrap().block,
        Some(hash)
    );
}

#[test]
fn blocks_leaving_the_best_chain() {
    let btc = Btc::from_env();
    let wallet = wallet();
    let c = client();
    let (addr, s) = fresh(&wallet);
    let tx = wallet.fund_and_sign(vec![pay(50_000, &s)]);
    assert_eq!(c.broadcast(&tx).unwrap(), Ok(()));
    let hash_hex = wallet.mine(1, &addr).remove(0);
    let hash = internal(&hash_hex);
    let (height, tip) = c.tip().unwrap();
    assert_eq!(tip, hash);
    let block = c.block(&hash).unwrap().unwrap();
    let coinbase = block.txdata[0].clone();

    btc.call("invalidateblock", &[json!(hash_hex)]);
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        assert_eq!(
            c.tip().unwrap(),
            (height - 1, block.header.prev_blockhash.to_byte_array())
        );
        assert_eq!(c.block_hash(height).unwrap(), None);
        let header = c.header(&hash).unwrap().unwrap();
        assert_eq!((header.height, header.in_best_chain), (height, false));
        assert_eq!(c.block(&hash).unwrap(), Some(block.clone()));
        // The block's transactions return to the mempool; the coinbase exists only in the block that left the best
        // chain and counts as unknown.
        assert_eq!(
            c.transaction(&txid(&tx)).unwrap(),
            Some(TxInfo {
                tx: tx.clone(),
                block: None
            })
        );
        assert!(c.in_mempool(&txid(&tx)).unwrap());
        assert_eq!(c.transaction(&txid(&coinbase)).unwrap(), None);
        assert_eq!(c.utxo(&out(&coinbase, 0), true).unwrap(), None);
        assert_eq!(c.utxo(&out(&tx, vout_of(&tx, &s)), false).unwrap(), None);
    }));
    // Restore the chain whether or not the assertions passed, so that other tests are not affected.
    btc.call("reconsiderblock", &[json!(hash_hex)]);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }

    assert_eq!(c.tip().unwrap(), (height, hash));
    assert!(c.header(&hash).unwrap().unwrap().in_best_chain);
    assert_eq!(
        c.transaction(&txid(&tx)).unwrap(),
        Some(TxInfo {
            tx: tx.clone(),
            block: Some(hash)
        })
    );
    assert!(!c.in_mempool(&txid(&tx)).unwrap());
    assert_eq!(
        c.transaction(&txid(&coinbase)).unwrap().unwrap().block,
        Some(hash)
    );
}

#[test]
fn connection_failures_are_local_faults() {
    let wrong_password = RpcClient::new(&rpc_url(), "ksana", "wrong").unwrap();
    assert!(matches!(wrong_password.tip(), Err(BtcError::Rpc(_))));
    // Take a free port and close it immediately: the connection is refused.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let closed = RpcClient::new(&format!("http://127.0.0.1:{port}"), "ksana", "ksana").unwrap();
    assert!(matches!(closed.tip(), Err(BtcError::Rpc(_))));
    assert!(matches!(closed.in_mempool(&[0; 32]), Err(BtcError::Rpc(_))));
}
