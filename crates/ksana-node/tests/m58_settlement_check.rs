//! The check of a block's settlement transactions with bitcoind when bitcoind learns them while the check runs (M58,
//! D-145), on an in-process node and the in-memory mock BTC chain.

mod common;

use std::sync::{Arc, Mutex};

use bitcoin::{OutPoint, Script, Transaction};
use common::mock_node::{signed, MockNode};
use common::user;
use ksana_btc::client::{BtcClient, BtcError, HeaderInfo, ScannedUtxo, TxInfo, UtxoInfo};
use ksana_btc::mock::MockBtc;
use ksana_btc::settlement::SettlementPair;
use ksana_node::chain::test_settlement;
use ksana_primitives::FeeMode;
use ksana_rpc::SettlementState;

/// bitcoind as a node sees it when transactions reach bitcoind right after the node looked them up: `transaction`
/// does not know the ones in `unseen`, and everything else answers from the mock chain as it is.
struct LateLookup {
    inner: Arc<MockBtc>,
    unseen: Mutex<Vec<[u8; 32]>>,
}

impl LateLookup {
    fn unseen(&self, txids: &[[u8; 32]]) {
        *self.unseen.lock().unwrap() = txids.to_vec();
    }
}

impl BtcClient for LateLookup {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        self.inner.tip()
    }
    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        self.inner.block_hash(height)
    }
    fn block(&self, hash: &[u8; 32]) -> Result<Option<bitcoin::Block>, BtcError> {
        self.inner.block(hash)
    }
    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        self.inner.header(hash)
    }
    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        if self.unseen.lock().unwrap().contains(txid) {
            return Ok(None);
        }
        self.inner.transaction(txid)
    }
    fn utxo(
        &self,
        outpoint: &OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        self.inner.utxo(outpoint, include_mempool)
    }
    fn scan_utxos(&self, script: &Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        self.inner.scan_utxos(script)
    }
    fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        self.inner.test_accept(txs)
    }
    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        self.inner.broadcast(tx)
    }
    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        self.inner.in_mempool(txid)
    }
    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        self.inner.mempool_txids()
    }
    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        self.inner.txindex_height()
    }
}

/// A node checks the settlement transactions of a block it receives with bitcoind: it looks both up, and has
/// `testmempoolaccept` check the ones bitcoind does not know. The nodes that have the block broadcast them once it is
/// `SETTLEMENT_DEPTH` deep, so for a node that receives the block about then (one behind the tip) bitcoind may learn
/// them between the lookups and the check, and then refuses them as `txn-already-in-mempool`, or `txn-already-known`
/// once a BTC block has them. Before D-145 the node took that for a rejection and did not accept the block for now.
#[test]
fn settlement_transactions_bitcoind_learns_during_the_check_are_accepted() {
    let (alice, bob) = (user("m58/alice"), user("m58/bob"));
    let n = MockNode::start(&[(&alice, 1_000)]);
    let op = signed(&alice, 0, FeeMode::Token { amount: 2 }, bob.account, 5);
    let included = n.wait_included(&n.submit(&op)).block.unwrap();
    n.wait_settlement(included.height, SettlementState::Broadcast);
    n.node().set_paused(true);
    let pair = SettlementPair::decode(
        n.block(included.height)
            .settlement
            .as_ref()
            .expect("the block carries a settlement transaction"),
    )
    .unwrap();
    let btc = LateLookup {
        inner: Arc::clone(&n.mock),
        unseen: Mutex::default(),
    };
    let both = [pair.commit_txid(), pair.txid()];
    btc.unseen(&both);

    // Both transactions arrived after the lookups: the package is refused at the commit transaction as known, and so
    // is the reveal transaction checked alone
    assert_eq!(
        n.mock.test_accept(&pair.txs()).unwrap(),
        Err("txn-already-in-mempool".to_string())
    );
    assert_eq!(test_settlement(&btc, &pair).unwrap(), Ok(()));

    // Only the commit transaction arrived: the reveal transaction is checked alone, and a valid one is accepted
    assert!(n.mock.evict(&pair.txid()));
    assert!(n.mock.in_mempool(&pair.commit_txid()).unwrap());
    assert_eq!(test_settlement(&btc, &pair).unwrap(), Ok(()));
    // ... while one bitcoind would not take is still refused, for its own reason
    let mut invalid = pair.clone();
    invalid.reveal.input[0].previous_output.vout = 7;
    let refused = test_settlement(&btc, &invalid).unwrap().unwrap_err();
    assert!(!refused.starts_with("txn-already"), "{refused}");
    // The same when the lookup finds the commit transaction
    btc.unseen(&[]);
    assert_eq!(test_settlement(&btc, &invalid).unwrap(), Err(refused));
    btc.unseen(&both);

    // A pair bitcoind does not know is checked as one package as before: one whose commit transaction it would not
    // take is refused
    assert!(n.mock.evict(&pair.commit_txid()));
    assert_eq!(test_settlement(&btc, &pair).unwrap(), Ok(()));
    let mut invalid = pair.clone();
    invalid.commit.input[0].previous_output.vout = 7;
    let refused = test_settlement(&btc, &invalid).unwrap().unwrap_err();
    assert!(!refused.starts_with("txn-already"), "{refused}");

    // A BTC block confirmed both between the lookup of the reveal transaction and that of the commit transaction:
    // the reveal transaction is checked alone and refused as confirmed
    n.mock.broadcast_all(&pair.txs()).unwrap().unwrap();
    n.mock.mine(1);
    btc.unseen(&[pair.txid()]);
    assert_eq!(
        n.mock.test_accept(&[&pair.reveal]).unwrap(),
        Err("txn-already-known".to_string())
    );
    assert_eq!(test_settlement(&btc, &pair).unwrap(), Ok(()));
}
