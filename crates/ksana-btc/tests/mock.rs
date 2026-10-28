//! `MockBtc` (feature `mock`): funding, submission and mining, conflicts, eviction by `mine_block`, UTXO queries with /
//! without the mempool, scan heights, headers and `in_best_chain`, reorgs (transactions return to the mempool),
//! consistency of block hashes and Merkle roots, determinism, fee estimates and mining by ancestor package fee rate
//! (CPFP).
#![cfg(feature = "mock")]

use std::collections::BTreeSet;
use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::script::Builder;
use bitcoin::{
    absolute, transaction, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxOut, Txid, WPubkeyHash, Witness,
};
use ksana_btc::client::{
    BtcClient, BtcError, HeaderInfo, MempoolInfo, ScannedUtxo, TxInfo, UtxoInfo,
};
use ksana_btc::mock::{
    coinbase_script, MockBtc, BLOCK_INTERVAL_SECS, COINBASE_MATURITY, COINBASE_VALUE_SAT,
    REGTEST_BITS,
};

/// Mutually distinct P2WPKH locking scripts.
fn script(tag: u8) -> ScriptBuf {
    ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([tag; 20]))
}

/// A v2 transaction spending `inputs` and paying `outputs` (sats, script) (the mock chain checks neither scripts nor
/// signatures).
fn spend(inputs: &[OutPoint], outputs: &[(u64, &ScriptBuf)]) -> Transaction {
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
        output: outputs
            .iter()
            .map(|(value, script)| TxOut {
                value: Amount::from_sat(*value),
                script_pubkey: (*script).clone(),
            })
            .collect(),
    }
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

fn err<T>(reason: &str) -> Result<T, String> {
    Err(reason.to_string())
}

#[test]
fn starts_at_the_regtest_genesis_block() {
    let btc = MockBtc::default();
    let genesis = bitcoin::constants::genesis_block(Network::Regtest);
    let hash = genesis.block_hash().to_byte_array();
    assert_eq!(btc.tip().unwrap(), (0, hash));
    assert_eq!((btc.tip_height(), btc.tip_hash()), (0, hash));
    assert_eq!(btc.block_hash(0).unwrap(), Some(hash));
    assert_eq!(btc.block_hash(1).unwrap(), None);
    assert_eq!(btc.block_hash(u32::MAX).unwrap(), None);
    assert_eq!(btc.block(&hash).unwrap(), Some(genesis.clone()));
    assert_eq!(
        btc.header(&hash).unwrap(),
        Some(HeaderInfo {
            height: 0,
            in_best_chain: true,
            header: genesis.header,
        })
    );
    assert_eq!(btc.block(&[0xaa; 32]).unwrap(), None);
    assert_eq!(btc.header(&[0xaa; 32]).unwrap(), None);
    // Same as bitcoind: the genesis coinbase is not an ordinary transaction and is not in the UTXO set.
    let coinbase = &genesis.txdata[0];
    assert_eq!(btc.transaction(&txid(coinbase)).unwrap(), None);
    assert_eq!(btc.utxo(&out(coinbase, 0), true).unwrap(), None);
    assert!(btc.mempool().is_empty());
}

#[test]
fn fund_mines_the_funding_transaction_alone() {
    let btc = MockBtc::new();
    let s = script(1);
    let ops = btc.fund(&s, &[1_000, 2_000]);
    assert_eq!(btc.tip_height(), 1);
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].txid, ops[1].txid);
    assert_eq!((ops[0].vout, ops[1].vout), (0, 1));

    let info = btc
        .transaction(&ops[0].txid.to_byte_array())
        .unwrap()
        .unwrap();
    assert_eq!(info.block, Some(btc.tip_hash()));
    assert_eq!(
        info.tx.output,
        vec![
            TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: s.clone()
            },
            TxOut {
                value: Amount::from_sat(2_000),
                script_pubkey: s.clone()
            },
        ]
    );
    let block = btc.block(&btc.tip_hash()).unwrap().unwrap();
    assert_eq!(block.txdata.len(), 2);
    assert_eq!(block.txdata[1], info.tx);
    assert_eq!(
        btc.utxo(&ops[1], false).unwrap(),
        Some(UtxoInfo {
            txout: info.tx.output[1].clone(),
            confirmations: 1,
            coinbase: false,
        })
    );
    btc.mine(2);
    assert_eq!(btc.utxo(&ops[1], true).unwrap().unwrap().confirmations, 3);

    // A funding block does not include mempool transactions; every funding transaction is different.
    let pending = spend(&[ops[0]], &[(900, &s)]);
    btc.submit(pending.clone()).unwrap();
    let again = btc.fund(&s, &[1_000, 2_000]);
    assert_ne!(again[0].txid, ops[0].txid);
    assert!(btc.in_mempool(&txid(&pending)).unwrap());
    assert_eq!(btc.mempool(), vec![pending]);

    // Invalid amounts: no block is mined.
    let height = btc.tip_height();
    assert!(btc.fund(&s, &[]).is_empty());
    assert!(btc.fund(&s, &[u64::MAX, 1]).is_empty());
    assert!(btc.fund(&s, &[Amount::MAX_MONEY.to_sat() + 1]).is_empty());
    assert_eq!(btc.tip_height(), height);
}

#[test]
fn submitted_transactions_are_mined_in_order() {
    let btc = MockBtc::new();
    let (a, b) = (script(1), script(2));
    let funded = btc.fund(&a, &[10_000]);
    let parent = spend(&[funded[0]], &[(6_000, &b), (3_000, &a)]);
    let parent_id = btc.submit(parent.clone()).unwrap();
    assert_eq!(parent_id, txid(&parent));
    assert!(btc.in_mempool(&parent_id).unwrap());
    assert_eq!(
        btc.transaction(&parent_id).unwrap(),
        Some(TxInfo {
            tx: parent.clone(),
            block: None
        })
    );

    // An output spent by a mempool transaction: still unspent when looking at the chain only, spent with the mempool.
    assert!(btc.utxo(&funded[0], false).unwrap().is_some());
    assert_eq!(btc.utxo(&funded[0], true).unwrap(), None);
    // New outputs of a mempool transaction: visible only with the mempool, 0 confirmations.
    assert_eq!(btc.utxo(&out(&parent, 0), false).unwrap(), None);
    assert_eq!(
        btc.utxo(&out(&parent, 0), true).unwrap(),
        Some(UtxoInfo {
            txout: parent.output[0].clone(),
            confirmations: 0,
            coinbase: false,
        })
    );
    assert_eq!(btc.utxo(&out(&parent, 2), true).unwrap(), None);

    // A child may spend an output in the mempool; scanning ignores the mempool.
    let child = spend(&[out(&parent, 0)], &[(5_000, &a)]);
    btc.submit(child.clone()).unwrap();
    assert_eq!(btc.mempool(), vec![parent.clone(), child.clone()]);
    assert!(btc.scan_utxos(&b).unwrap().is_empty());

    let hashes = btc.mine(3);
    assert_eq!(hashes.len(), 3);
    assert_eq!(btc.tip_height(), 4);
    assert_eq!(btc.tip_hash(), hashes[2]);
    let block = btc.block(&hashes[0]).unwrap().unwrap();
    assert_eq!(block.txdata[1..], [parent.clone(), child.clone()]);
    assert_eq!(btc.block(&hashes[1]).unwrap().unwrap().txdata.len(), 1);
    assert!(btc.mempool().is_empty());
    assert!(!btc.in_mempool(&parent_id).unwrap());
    assert_eq!(
        btc.transaction(&txid(&child)).unwrap().unwrap().block,
        Some(hashes[0])
    );
    assert_eq!(
        btc.utxo(&out(&child, 0), false)
            .unwrap()
            .unwrap()
            .confirmations,
        3
    );
    assert_eq!(btc.utxo(&out(&parent, 0), true).unwrap(), None);
    assert_eq!(btc.utxo(&funded[0], false).unwrap(), None);
    assert_eq!(
        btc.scan_utxos(&b).unwrap(),
        Vec::<ScannedUtxo>::new(),
        "b's output was spent by child"
    );
}

#[test]
fn rejections_use_bitcoin_core_reasons() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[10_000, 10_000]);
    let tx = spend(&[f[0]], &[(9_000, &s)]);

    // test_accept does not add to the mempool; broadcast does.
    assert_eq!(btc.test_accept(&[&tx]).unwrap(), Ok(()));
    assert!(btc.mempool().is_empty());
    assert_eq!(btc.broadcast(&tx).unwrap(), Ok(()));
    assert!(btc.in_mempool(&txid(&tx)).unwrap());
    assert_eq!(btc.submit(tx.clone()), err("txn-already-in-mempool"));
    assert_eq!(
        btc.test_accept(&[&tx]).unwrap(),
        err("txn-already-in-mempool")
    );
    assert_eq!(btc.broadcast(&tx).unwrap(), Ok(()));

    // Conflict with the mempool: rejected even with a higher fee (no RBF support).
    let conflict = spend(&[f[0]], &[(1_000, &s)]);
    assert_eq!(btc.submit(conflict.clone()), err("txn-mempool-conflict"));
    assert_eq!(
        btc.test_accept(&[&conflict]).unwrap(),
        err("txn-mempool-conflict")
    );
    assert_eq!(
        btc.broadcast(&conflict).unwrap(),
        err("txn-mempool-conflict")
    );

    let missing = OutPoint {
        txid: Txid::from_byte_array([9; 32]),
        vout: 0,
    };
    let cases = [
        (
            spend(&[missing], &[(1, &s)]),
            "bad-txns-inputs-missingorspent",
        ),
        (
            spend(&[out(&tx, 1)], &[(1, &s)]),
            "bad-txns-inputs-missingorspent",
        ),
        (spend(&[f[1]], &[(10_001, &s)]), "bad-txns-in-belowout"),
        (
            spend(&[f[1], f[1]], &[(1, &s)]),
            "bad-txns-inputs-duplicate",
        ),
        (spend(&[], &[(1, &s)]), "bad-txns-vin-empty"),
        (spend(&[f[1]], &[]), "bad-txns-vout-empty"),
        (spend(&[OutPoint::null()], &[(1, &s)]), "coinbase"),
        (
            spend(&[f[1], OutPoint::null()], &[(1, &s)]),
            "bad-txns-prevout-null",
        ),
        (
            spend(&[f[1]], &[(Amount::MAX_MONEY.to_sat() + 1, &s)]),
            "bad-txns-vout-toolarge",
        ),
    ];
    for (bad, reason) in cases {
        assert_eq!(btc.test_accept(&[&bad]).unwrap(), err(reason), "{reason}");
        assert_eq!(btc.broadcast(&bad).unwrap(), err(reason), "{reason}");
        assert_eq!(btc.submit(bad), err(reason), "{reason}");
    }
    assert_eq!(btc.mempool(), vec![tx.clone()]);

    // OP_RETURN outputs are unspendable and do not enter the UTXO set.
    let op_return = Builder::new()
        .push_opcode(OP_RETURN)
        .push_slice(b"data")
        .into_script();
    let data = spend(&[f[1]], &[(0, &op_return), (9_000, &s)]);
    btc.submit(data.clone()).unwrap();
    assert_eq!(btc.utxo(&out(&data, 0), true).unwrap(), None);
    assert_eq!(
        btc.submit(spend(&[out(&data, 0)], &[(0, &s)])),
        err("bad-txns-inputs-missingorspent")
    );

    // A confirmed transaction.
    btc.mine(1);
    assert_eq!(btc.submit(tx.clone()), err("txn-already-known"));
    assert_eq!(btc.test_accept(&[&tx]).unwrap(), err("txn-already-known"));
    assert_eq!(btc.broadcast(&tx).unwrap(), Ok(()));
    assert_eq!(btc.utxo(&out(&data, 0), false).unwrap(), None);
    assert!(btc.utxo(&out(&data, 1), false).unwrap().is_some());
    assert!(btc.mempool().is_empty());
}

#[test]
fn coinbase_outputs_mature_after_100_blocks() {
    let btc = MockBtc::new();
    let hash = btc.mine(1)[0];
    let block = btc.block(&hash).unwrap().unwrap();
    let coinbase = &block.txdata[0];
    assert!(coinbase.is_coinbase());
    assert_eq!(
        coinbase.output[0],
        TxOut {
            value: Amount::from_sat(COINBASE_VALUE_SAT),
            script_pubkey: coinbase_script(),
        }
    );
    let op = out(coinbase, 0);
    let info = btc.utxo(&op, false).unwrap().unwrap();
    assert!(info.coinbase);
    assert_eq!(info.confirmations, 1);
    assert_eq!(
        btc.scan_utxos(&coinbase_script()).unwrap(),
        vec![ScannedUtxo {
            outpoint: op,
            txout: coinbase.output[0].clone(),
            height: 1,
            coinbase: true,
        }]
    );
    // The witness commitment output (OP_RETURN) is not a UTXO.
    assert_eq!(btc.utxo(&out(coinbase, 1), false).unwrap(), None);
    assert_eq!(
        btc.transaction(&txid(coinbase)).unwrap().unwrap().block,
        Some(hash)
    );

    let s = script(1);
    let tx = spend(&[op], &[(COINBASE_VALUE_SAT - 1_000, &s)]);
    // Tip 99: the next block is height 100, only 99 confirmations.
    btc.mine(COINBASE_MATURITY - 2);
    assert_eq!(
        btc.submit(tx.clone()),
        err("bad-txns-premature-spend-of-coinbase")
    );
    assert_eq!(
        btc.mine_block(vec![tx.clone()]),
        err("bad-txns-premature-spend-of-coinbase")
    );
    assert_eq!(btc.tip_height(), COINBASE_MATURITY - 1);
    btc.mine(1);
    btc.submit(tx.clone()).unwrap();
    // When a reorg makes it immature again, the transaction spending it leaves the mempool.
    btc.reorg(1);
    assert!(btc.mempool().is_empty());
    btc.mine(1);
    assert_eq!(btc.mine_block(vec![tx.clone()]).map(|_| ()), Ok(()));
    assert_eq!(btc.utxo(&op, false).unwrap(), None);
}

#[test]
fn mine_block_evicts_conflicting_mempool_transactions() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[10_000, 20_000, 30_000]);
    let a = spend(&[f[0]], &[(9_000, &s)]);
    let a_child = spend(&[out(&a, 0)], &[(8_000, &s)]);
    let other = spend(&[f[1]], &[(19_000, &s)]);
    let untouched = spend(&[f[2]], &[(29_000, &s)]);
    for tx in [&a, &a_child, &other, &untouched] {
        btc.submit(tx.clone()).unwrap();
    }
    let b = spend(&[f[0]], &[(9_500, &s)]);

    // Transactions in a block cannot spend mempool outputs; duplicates and double spends within the block are invalid,
    // and no block is mined on failure.
    assert_eq!(
        btc.mine_block(vec![a_child.clone()]),
        err("bad-txns-inputs-missingorspent")
    );
    assert_eq!(
        btc.mine_block(vec![b.clone(), b.clone()]),
        err("bad-txns-duplicate")
    );
    let b2 = spend(&[f[0]], &[(9_400, &s)]);
    assert_eq!(
        btc.mine_block(vec![b.clone(), b2]),
        err("bad-txns-inputs-missingorspent")
    );
    assert_eq!(
        btc.mine_block(vec![spend(&[], &[(1, &s)])]),
        err("bad-txns-vin-empty")
    );
    assert_eq!(btc.tip_height(), 1);
    assert_eq!(btc.mempool().len(), 4);

    let hash = btc.mine_block(vec![b.clone(), other.clone()]).unwrap();
    assert_eq!(btc.tip_hash(), hash);
    let block = btc.block(&hash).unwrap().unwrap();
    assert_eq!(block.txdata[1..], [b.clone(), other.clone()]);
    // a and its descendants are evicted; other was mined; untouched stays in the mempool.
    assert_eq!(btc.mempool(), vec![untouched.clone()]);
    assert_eq!(btc.transaction(&txid(&a)).unwrap(), None);
    assert_eq!(btc.transaction(&txid(&a_child)).unwrap(), None);
    assert_eq!(
        btc.transaction(&txid(&other)).unwrap().unwrap().block,
        Some(hash)
    );

    // Transactions within a block may spend outputs of earlier transactions in the same block.
    let c = spend(&[out(&b, 0)], &[(9_000, &s)]);
    let d = spend(&[out(&c, 0)], &[(8_000, &s)]);
    btc.mine_block(vec![c.clone(), d.clone()]).unwrap();
    assert_eq!(btc.utxo(&out(&c, 0), false).unwrap(), None);
    assert!(btc.utxo(&out(&d, 0), false).unwrap().is_some());
    assert_eq!(btc.mine_block(vec![d]), err("bad-txns-BIP30"));
    // An empty block.
    let empty = btc.mine_block(vec![]).unwrap();
    assert_eq!(btc.block(&empty).unwrap().unwrap().txdata.len(), 1);
    assert_eq!(btc.mempool(), vec![untouched]);
}

#[test]
fn scan_utxos_reports_confirmed_outputs_with_heights() {
    let btc = MockBtc::new();
    let s = script(7);
    assert!(btc.scan_utxos(&s).unwrap().is_empty());
    let first = btc.fund(&s, &[1_000]);
    btc.mine(2);
    let second = btc.fund(&s, &[2_000, 3_000]);
    btc.fund(&script(8), &[4_000]);
    let summary = |btc: &MockBtc| {
        btc.scan_utxos(&s)
            .unwrap()
            .into_iter()
            .map(|u| {
                assert_eq!(u.txout.script_pubkey, s);
                assert!(!u.coinbase);
                (u.outpoint, u.height, u.txout.value.to_sat())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        summary(&btc),
        vec![
            (first[0], 1, 1_000),
            (second[0], 4, 2_000),
            (second[1], 4, 3_000)
        ]
    );

    // Neither spends nor new outputs in the mempool affect scanning; they show up only once confirmed.
    let tx = spend(&[second[0]], &[(1_500, &s)]);
    btc.submit(tx.clone()).unwrap();
    assert_eq!(summary(&btc).len(), 3);
    btc.mine(1);
    assert_eq!(
        summary(&btc),
        vec![
            (first[0], 1, 1_000),
            (second[1], 4, 3_000),
            (out(&tx, 0), 6, 1_500)
        ]
    );
}

#[test]
fn reorg_returns_transactions_to_the_mempool() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[10_000, 20_000]);
    let b1 = btc.tip_hash();
    let t = spend(&[f[0]], &[(9_000, &s)]);
    btc.submit(t.clone()).unwrap();
    let b2 = btc.mine(1)[0];
    let late = spend(&[out(&t, 0)], &[(8_000, &s)]);
    btc.submit(late.clone()).unwrap();
    let b3 = btc.mine(1)[0];
    let pending = spend(&[f[1]], &[(19_000, &s)]);
    btc.submit(pending.clone()).unwrap();
    let coinbase2 = btc.block(&b2).unwrap().unwrap().txdata[0].clone();
    assert!(btc.header(&b2).unwrap().unwrap().in_best_chain);

    assert_eq!(btc.reorg(2), vec![b2, b3]);
    assert_eq!(btc.tip().unwrap(), (1, b1));
    assert_eq!(btc.block_hash(2).unwrap(), None);
    for (hash, height) in [(b2, 2), (b3, 3)] {
        let header = btc.header(&hash).unwrap().unwrap();
        assert_eq!((header.height, header.in_best_chain), (height, false));
        assert_eq!(
            btc.block(&hash).unwrap().unwrap().header,
            header.header,
            "disconnected blocks remain queryable"
        );
    }
    // Transactions of the disconnected blocks return to the front of the mempool in their original order, the previous
    // mempool transactions follow; the coinbases disappear.
    assert_eq!(
        btc.mempool(),
        vec![t.clone(), late.clone(), pending.clone()]
    );
    assert_eq!(btc.transaction(&txid(&t)).unwrap().unwrap().block, None);
    assert_eq!(btc.transaction(&txid(&coinbase2)).unwrap(), None);
    assert_eq!(btc.utxo(&out(&coinbase2, 0), true).unwrap(), None);
    // The UTXO set is restored to its state after block 1.
    assert!(btc.utxo(&f[0], false).unwrap().is_some());
    assert_eq!(btc.utxo(&f[0], true).unwrap(), None);
    assert_eq!(btc.utxo(&out(&t, 0), false).unwrap(), None);
    assert_eq!(btc.utxo(&out(&t, 0), true).unwrap(), None, "spent by late");

    // Mine another branch: the new blocks differ from the disconnected ones, the transactions confirm on the new
    // branch.
    let branch = btc.mine(3);
    assert_ne!(branch[0], b2);
    assert_ne!(branch[1], b3);
    assert_eq!(btc.tip_height(), 4);
    assert_eq!(btc.block_hash(2).unwrap(), Some(branch[0]));
    assert!(btc.header(&branch[0]).unwrap().unwrap().in_best_chain);
    assert!(!btc.header(&b2).unwrap().unwrap().in_best_chain);
    for tx in [&t, &late, &pending] {
        assert_eq!(
            btc.transaction(&txid(tx)).unwrap().unwrap().block,
            Some(branch[0])
        );
    }
    assert!(btc.mempool().is_empty());
}

#[test]
fn reorg_drops_transactions_that_conflict_with_the_new_branch() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[10_000]);
    let t = spend(&[f[0]], &[(9_000, &s)]);
    btc.submit(t.clone()).unwrap();
    btc.mine(1);
    btc.reorg(1);
    assert_eq!(btc.mempool(), vec![t.clone()]);
    let rival = spend(&[f[0]], &[(9_999, &s)]);
    let rival_block = btc.mine_block(vec![rival.clone()]).unwrap();
    assert!(btc.mempool().is_empty());
    assert_eq!(btc.transaction(&txid(&t)).unwrap(), None);

    // Disconnect the funding block too: the funding transaction's virtual input is restored with it, and the funding
    // transaction and rival return to the mempool in order.
    let funding = f[0].txid.to_byte_array();
    assert_eq!(btc.reorg(2).len(), 2);
    assert_eq!(btc.tip_height(), 0);
    assert_eq!(
        btc.mempool().iter().map(txid).collect::<Vec<_>>(),
        vec![funding, txid(&rival)]
    );
    assert!(!btc.header(&rival_block).unwrap().unwrap().in_best_chain);
    let hash = btc.mine(1)[0];
    assert_eq!(
        btc.transaction(&funding).unwrap().unwrap().block,
        Some(hash)
    );
    assert_eq!(
        btc.transaction(&txid(&rival)).unwrap().unwrap().block,
        Some(hash)
    );
}

/// BTC goes back to a branch it left: the disconnected blocks connect again in order on top of the fork point, their
/// transactions confirm again and leave the mempool, and only a child of the tip can be connected.
#[test]
fn reconnect_goes_back_to_a_branch_left_by_a_reorg() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[10_000, 20_000]);
    let t = spend(&[f[0]], &[(9_000, &s)]);
    btc.submit(t.clone()).unwrap();
    let old = btc.mine(2);
    assert_eq!(btc.reorg(2), old);
    // the other branch confirms t again
    let other = btc.mine(3);
    assert_eq!(
        btc.transaction(&txid(&t)).unwrap().unwrap().block,
        Some(other[0])
    );
    assert!(btc.reconnect(&old).is_err(), "not a child of the tip");
    assert_eq!(btc.tip_height(), 4);

    let pending = spend(&[f[1]], &[(19_000, &s)]);
    btc.submit(pending.clone()).unwrap();
    assert_eq!(btc.reorg(3), other);
    btc.reconnect(&old).unwrap();
    assert_eq!(btc.tip().unwrap(), (3, old[1]));
    assert_eq!(btc.block_hash(2).unwrap(), Some(old[0]));
    assert!(btc.header(&old[0]).unwrap().unwrap().in_best_chain);
    assert!(!btc.header(&other[0]).unwrap().unwrap().in_best_chain);
    assert_eq!(
        btc.transaction(&txid(&t)).unwrap().unwrap().block,
        Some(old[0])
    );
    assert_eq!(btc.mempool(), vec![pending.clone()]);
    assert!(btc.utxo(&out(&t, 0), false).unwrap().is_some());
    // a new block extends the old branch
    let next = btc.mine(1)[0];
    assert_eq!(
        btc.block(&next)
            .unwrap()
            .unwrap()
            .header
            .prev_blockhash
            .to_byte_array(),
        old[1]
    );
    assert!(btc.mempool().is_empty());
}

#[test]
fn reorg_stops_above_the_genesis_block() {
    let btc = MockBtc::new();
    let genesis = btc.tip_hash();
    let mined = btc.mine(2);
    assert_eq!(btc.reorg(5), mined);
    assert_eq!(btc.tip().unwrap(), (0, genesis));
    assert!(btc.reorg(1).is_empty());
    assert!(btc.header(&genesis).unwrap().unwrap().in_best_chain);
}

#[test]
fn blocks_hashes_and_merkle_roots_are_consistent() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[50_000]);
    // A transaction with a witness: the coinbase's witness commitment must cover it.
    let mut t = spend(&[f[0]], &[(40_000, &s)]);
    t.input[0].witness = Witness::from_slice(&[vec![1u8; 72], vec![2u8; 33]]);
    btc.submit(t.clone()).unwrap();
    btc.mine(20);
    let stale = btc.reorg(3);
    btc.mine(4);
    let tip = btc.tip_height();
    assert_eq!(tip, 22);

    let mut coinbases = BTreeSet::new();
    let mut prev_time = 0;
    for height in 0..=tip {
        let hash = btc.block_hash(height).unwrap().unwrap();
        let block = btc.block(&hash).unwrap().unwrap();
        assert_eq!(block.block_hash().to_byte_array(), hash);
        assert!(block.check_merkle_root(), "height {height}");
        assert!(block.check_witness_commitment(), "height {height}");
        assert_eq!(block.header.bits.to_consensus(), REGTEST_BITS);
        assert!(block.header.time > prev_time);
        prev_time = block.header.time;
        assert_eq!(
            btc.header(&hash).unwrap(),
            Some(HeaderInfo {
                height,
                in_best_chain: true,
                header: block.header,
            })
        );
        if height == 0 {
            continue;
        }
        let parent = btc.block_hash(height - 1).unwrap().unwrap();
        assert_eq!(block.header.prev_blockhash.to_byte_array(), parent);
        assert_eq!(block.header.nonce, 0);
        let coinbase = &block.txdata[0];
        assert!(coinbase.is_coinbase());
        let bip34 = Builder::new().push_int(height.into()).into_script();
        assert!(coinbase.input[0]
            .script_sig
            .as_bytes()
            .starts_with(bip34.as_bytes()));
        if height > 16 {
            assert_eq!(block.bip34_block_height().unwrap(), u64::from(height));
        }
        assert!(coinbases.insert(coinbase.compute_txid()));
        assert_eq!(
            btc.transaction(&txid(coinbase)).unwrap().unwrap().block,
            Some(hash)
        );
    }
    assert!(btc
        .block(&btc.block_hash(2).unwrap().unwrap())
        .unwrap()
        .unwrap()
        .txdata
        .contains(&t));

    // The disconnected blocks (heights 19–21) are consistent as well; their coinbases differ from those at the same
    // heights on the new branch.
    for (height, hash) in (19u32..).zip(&stale) {
        let block = btc.block(hash).unwrap().unwrap();
        let header = btc.header(hash).unwrap().unwrap();
        assert_eq!(header.height, height);
        assert!(!header.in_best_chain);
        assert!(block.check_merkle_root());
        assert!(block.check_witness_commitment());
        assert!(coinbases.insert(block.txdata[0].compute_txid()));
        assert_ne!(btc.block_hash(height).unwrap(), Some(*hash));
    }
    // Time only increases: the first block of the new branch is later than the last disconnected block.
    let first_new = btc
        .block(&btc.block_hash(19).unwrap().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        first_new.header.time,
        btc.block(&stale[2]).unwrap().unwrap().header.time + BLOCK_INTERVAL_SECS
    );
}

#[test]
fn identical_operations_give_identical_chains() {
    fn run() -> Vec<[u8; 32]> {
        let btc = MockBtc::new();
        let s = script(3);
        let f = btc.fund(&s, &[5_000, 6_000]);
        btc.submit(spend(&[f[0]], &[(4_000, &s)])).unwrap();
        btc.mine(2);
        btc.reorg(1);
        btc.submit(spend(&[f[1]], &[(5_000, &s)])).unwrap();
        btc.mine(2);
        (0..=btc.tip_height())
            .map(|h| btc.block_hash(h).unwrap().unwrap())
            .collect()
    }
    let first = run();
    assert_eq!(first.len(), 5);
    assert_eq!(first, run());
}

#[test]
fn evict_and_clear_mempool() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[10_000, 10_000]);
    let a = spend(&[f[0]], &[(9_000, &s)]);
    let a_child = spend(&[out(&a, 0)], &[(8_000, &s)]);
    let b = spend(&[f[1]], &[(9_000, &s)]);
    for tx in [&a, &a_child, &b] {
        btc.submit(tx.clone()).unwrap();
    }
    assert!(btc.evict(&txid(&a)));
    assert_eq!(
        btc.mempool(),
        vec![b.clone()],
        "descendants are evicted too"
    );
    assert!(!btc.evict(&txid(&a)));
    btc.clear_mempool();
    assert!(btc.mempool().is_empty());
    assert!(!btc.in_mempool(&txid(&b)).unwrap());
    // Rebroadcast.
    assert_eq!(btc.broadcast(&a).unwrap(), Ok(()));
    assert_eq!(btc.broadcast(&a_child).unwrap(), Ok(()));
    assert_eq!(btc.mempool(), vec![a, a_child]);
}

#[test]
fn test_accept_checks_a_package_without_keeping_it() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[10_000]);
    let parent = spend(&[f[0]], &[(9_000, &s)]);
    let child = spend(&[out(&parent, 0)], &[(8_000, &s)]);
    // The child checked alone: the parent is not in the mempool
    assert_eq!(
        btc.test_accept(&[&child]).unwrap(),
        err("bad-txns-inputs-missingorspent")
    );
    // Accepted as a package (parent first), the mempool is unchanged; the reverse order is not accepted
    assert_eq!(btc.test_accept(&[&parent, &child]).unwrap(), Ok(()));
    assert!(btc.mempool().is_empty());
    assert_eq!(
        btc.test_accept(&[&child, &parent]).unwrap(),
        err("bad-txns-inputs-missingorspent")
    );
    // Once the parent is in the mempool, checking the child alone is enough
    btc.submit(parent.clone()).unwrap();
    assert_eq!(btc.test_accept(&[&child]).unwrap(), Ok(()));
    assert_eq!(btc.mempool(), vec![parent.clone()]);
    // broadcast_all broadcasts in order (one already in the mempool counts as success)
    assert_eq!(btc.broadcast_all(&[&parent, &child]).unwrap(), Ok(()));
    assert_eq!(btc.mempool(), vec![parent, child]);
}

#[test]
fn a_minimum_fee_rate_leaves_cheap_packages_until_a_child_pays_for_them() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[100_000, 100_000]);
    // vsize of a one-input one-output transaction without witness: 82 vB (10 + 41 + 31)
    let cheap = spend(&[f[0]], &[(100_000 - 82, &s)]);
    let rich = spend(&[f[1]], &[(100_000 - 82 * 20, &s)]);
    assert_eq!(cheap.weight().to_vbytes_ceil(), 82);
    btc.submit(cheap.clone()).unwrap();
    btc.submit(rich.clone()).unwrap();
    btc.set_min_feerate(10);
    let block = btc.mine(1)[0];
    assert_eq!(btc.block(&block).unwrap().unwrap().txdata[1..], [rich]);
    assert_eq!(btc.mempool(), vec![cheap.clone()]);
    // A child paying 19 sat/vB on its own 82 vB lifts the package of 164 vB to 10 sat/vB: both are mined, parent first
    let child = spend(&[out(&cheap, 0)], &[(100_000 - 82 - 82 * 19, &s)]);
    btc.submit(child.clone()).unwrap();
    let block = btc.mine(1)[0];
    assert_eq!(
        btc.block(&block).unwrap().unwrap().txdata[1..],
        [cheap.clone(), child.clone()]
    );
    assert!(btc.mempool().is_empty());

    // A held parent keeps its descendants out whatever they pay
    let g = btc.fund(&s, &[100_000]);
    let held = spend(&[g[0]], &[(100_000 - 82 * 10, &s)]);
    let rich_child = spend(&[out(&held, 0)], &[(100_000 - 82 * 100, &s)]);
    btc.hold(&txid(&held));
    btc.submit(held.clone()).unwrap();
    btc.submit(rich_child.clone()).unwrap();
    btc.mine(1);
    assert_eq!(btc.mempool(), vec![held.clone(), rich_child]);
    btc.release(&txid(&held));
    btc.set_min_feerate(0);
    btc.mine(1);
    assert!(btc.mempool().is_empty());
}

#[test]
fn fee_estimates_follow_the_targets_set() {
    let btc = MockBtc::new();
    assert_eq!(btc.estimate_feerate(2).unwrap(), None);
    btc.set_fee_estimate(1, Some(30));
    btc.set_fee_estimate(6, Some(5));
    assert_eq!(btc.estimate_feerate(1).unwrap(), Some(30));
    assert_eq!(btc.estimate_feerate(3).unwrap(), Some(30));
    assert_eq!(btc.estimate_feerate(6).unwrap(), Some(5));
    assert_eq!(btc.estimate_feerate(144).unwrap(), Some(5));
    btc.set_fee_estimate(1, None);
    assert_eq!(btc.estimate_feerate(3).unwrap(), None);
    // Through Arc as a trait object as well
    let client: Arc<dyn BtcClient> = Arc::new(btc);
    assert_eq!(client.estimate_feerate(10).unwrap(), Some(5));
}

#[test]
fn the_mempool_info_is_what_the_test_set() {
    let btc = MockBtc::new();
    assert_eq!(btc.mempool_info().unwrap(), None);
    let info = MempoolInfo {
        vsize: 314_541,
        min_feerate: 1,
    };
    btc.set_mempool_info(Some(info));
    assert_eq!(btc.mempool_info().unwrap(), Some(info));
    btc.set_offline(true);
    assert!(btc.mempool_info().is_err());
    btc.set_offline(false);
    btc.set_mempool_info(None);
    // Through Arc as a trait object as well
    let client: Arc<dyn BtcClient> = Arc::new(btc);
    assert_eq!(client.mempool_info().unwrap(), None);
}

#[test]
fn offline_mode_is_a_local_fault() {
    let btc = MockBtc::new();
    let s = script(1);
    let f = btc.fund(&s, &[1_000]);
    let tx = spend(&[f[0]], &[(500, &s)]);
    btc.set_offline(true);
    let offline = |r: Result<(), BtcError>| assert!(matches!(r, Err(BtcError::Rpc(_))));
    offline(btc.tip().map(|_| ()));
    offline(btc.block_hash(0).map(|_| ()));
    offline(btc.block(&[0; 32]).map(|_| ()));
    offline(btc.header(&[0; 32]).map(|_| ()));
    offline(btc.transaction(&[0; 32]).map(|_| ()));
    offline(btc.utxo(&f[0], true).map(|_| ()));
    offline(btc.scan_utxos(&s).map(|_| ()));
    offline(btc.test_accept(&[&tx]).map(|_| ()));
    offline(btc.broadcast(&tx).map(|_| ()));
    offline(btc.in_mempool(&[0; 32]).map(|_| ()));
    assert!(
        btc.mempool().is_empty(),
        "broadcasts while offline did not take effect"
    );
    // The helper methods that simulate the BTC network itself are unaffected.
    btc.mine(1);
    btc.set_offline(false);
    assert_eq!(btc.tip().unwrap().0, 2);
}

#[test]
fn usable_through_arc_and_trait_objects() {
    fn height(client: impl BtcClient) -> u32 {
        client.tip().unwrap().0
    }
    let btc = Arc::new(MockBtc::new());
    let client: Arc<dyn BtcClient> = btc.clone();
    btc.mine(1);
    assert_eq!(client.tip().unwrap(), (1, btc.tip_hash()));
    assert_eq!(height(btc.clone()), 1);
    assert_eq!(height(client.clone()), 1);
    let shared = Arc::clone(&btc);
    std::thread::spawn(move || shared.mine(1)).join().unwrap();
    assert_eq!(height(client), 2);
}
