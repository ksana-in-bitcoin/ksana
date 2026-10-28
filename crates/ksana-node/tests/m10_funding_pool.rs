//! M10: funding UTXO pool (D-075, local policy). Settlement transactions can only be funded by confirmed UTXOs and
//! cannot spend each other, so within one BTC block interval a producer can fund no more settlement transactions than
//! it has confirmed UTXOs. When fewer funding UTXOs than the target are available, the node splits the largest one into
//! several equal outputs (each at least `FUNDING_SPLIT_MIN_SAT`) and does not split again until the split transaction
//! confirms; the split UTXOs fund settlement transactions as usual, so when blocks are produced back to back within one
//! BTC block interval every block's settlement transaction has a UTXO available.

mod common;

use std::time::Duration;

use common::mock_node::{signed, wait, MockNode};
use common::user;
use ksana_btc::client::BtcClient;
use ksana_node::rescue::FUNDING_SPLIT_MIN_SAT;
use ksana_primitives::FeeMode;
use ksana_rpc::SettlementState;

#[test]
fn the_node_splits_its_largest_funding_utxo_until_the_pool_is_full() {
    let (alice, bob) = (user("m10/pool/alice"), user("m10/pool/bob"));
    let n = MockNode::start_custom(
        &[(&alice, 1_000)],
        Duration::from_millis(100),
        |mock| mock,
        // Every block holding ops carries its own settlement transaction (settlement batching off, D-077), so that
        // funding UTXOs become the bottleneck
        |funding| funding.with_pool_target(16).with_batching(false),
    );
    let script = n.funding.script.clone();
    let utxos = || n.mock.scan_utxos(&script).unwrap();
    assert_eq!(
        utxos().len(),
        8,
        "the fixture gives the funding address 8 UTXOs of 500 000 sats"
    );

    // No further split while a split transaction is in the mempool: at most one at any time
    let pending = wait("funding split transaction", || {
        let splits: Vec<_> = n
            .mock
            .mempool_txids()
            .unwrap()
            .into_iter()
            .filter_map(|txid| n.mock.transaction(&txid).unwrap())
            .filter(|info| {
                info.tx.output.len() >= 2
                    && info.tx.output.iter().all(|o| o.script_pubkey == script)
            })
            .collect();
        assert!(
            splits.len() <= 1,
            "no further split before the split transaction confirms"
        );
        splits.into_iter().next()
    });
    assert!(pending
        .tx
        .output
        .iter()
        .all(|o| o.value.to_sat() >= FUNDING_SPLIT_MIN_SAT));

    // With each BTC block mined, the split transaction confirms and the next one is split, until the available UTXOs
    // reach the target
    let pool = wait("funding UTXO pool refilled", || {
        n.mock.mine(1);
        std::thread::sleep(Duration::from_millis(300));
        let count = utxos().len();
        (count >= 16).then_some(count)
    });
    assert!(pool >= 16);

    // The split UTXOs fund settlement transactions as usual: 12 consecutive blocks each holding one op (more than the
    // fixture's original 8 UTXOs) with no BTC block in between; every block's settlement transaction can be funded and
    // broadcast
    let btc_tip = n.mock.tip().unwrap().0;
    let mut heights = Vec::new();
    for nonce in 0..12 {
        let id = n.submit(&signed(
            &alice,
            nonce,
            FeeMode::Token { amount: 5 },
            bob.account,
            1,
        ));
        heights.push(n.wait_included(&id).block.unwrap().height);
    }
    assert!(heights.windows(2).all(|w| w[0] < w[1]), "{heights:?}");
    for &height in &heights {
        n.wait_settlement(height, SettlementState::Broadcast);
    }
    assert_eq!(
        n.mock.tip().unwrap().0,
        btc_tip,
        "no BTC block was mined in between"
    );
    n.mock.mine(1);
    for height in heights {
        n.wait_settlement(height, SettlementState::Confirmed);
    }
}

/// Reaches the mock chain, except that UTXO set scans fail while `failing` is set (bitcoind busy with another
/// process's scan, or a scan slower than the RPC client's timeout); counts the scans asked for.
struct FailingScans {
    inner: std::sync::Arc<ksana_btc::mock::MockBtc>,
    failing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    scans: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

mod failing_scans {
    use bitcoin::{OutPoint, Script, Transaction};
    use ksana_btc::client::{BtcClient, BtcError, HeaderInfo, ScannedUtxo, TxInfo, UtxoInfo};
    use std::sync::atomic::Ordering;

    impl BtcClient for super::FailingScans {
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
            self.scans.fetch_add(1, Ordering::SeqCst);
            if self.failing.load(Ordering::SeqCst) {
                return Err(BtcError::Rpc(
                    "scantxoutset: Scan already in progress, use action \"abort\" or \"status\""
                        .into(),
                ));
            }
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
}

/// A failed scan of the funding address costs the producer its settlements, not its blocks (D-109): blocks go on and
/// include ops, the producer asks again only at the next anchor or after a pause, and once a scan works the bodies
/// that waited are settled (D-077).
#[test]
fn a_failed_funding_scan_skips_settlements_not_blocks() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    let (alice, bob) = (user("m10/scan/alice"), user("m10/scan/bob"));
    let failing = Arc::new(AtomicBool::new(true));
    let scans = Arc::new(AtomicU32::new(0));
    let (f, s) = (Arc::clone(&failing), Arc::clone(&scans));
    let n = MockNode::start_custom(
        &[(&alice, 1_000)],
        Duration::from_millis(100),
        move |mock| {
            Arc::new(FailingScans {
                inner: mock,
                failing: f,
                scans: s,
            })
        },
        // no splitting, as on the public signet: the pool would scan every round
        |funding| funding.with_pool_target(0),
    );

    let id = n.submit(&signed(
        &alice,
        0,
        FeeMode::Token { amount: 5 },
        bob.account,
        3,
    ));
    let height = n.wait_included(&id).block.unwrap().height;
    assert_eq!(n.ksa(&bob), 3, "the op took effect on the fast lane");
    // Blocks keep coming on the same BTC tip, without asking bitcoind to scan before each one
    let tip = n.chain_info().tip.height;
    wait("blocks after the failed scan", || {
        (n.chain_info().tip.height >= tip + 5).then_some(())
    });
    assert!(
        scans.load(Ordering::SeqCst) <= 2,
        "{} scans: one failed scan pauses them",
        scans.load(Ordering::SeqCst)
    );
    assert_eq!(n.settlement(height).status, SettlementState::Pending);

    // Scans work again: from the next anchor on, the waiting body is settled
    failing.store(false, Ordering::SeqCst);
    n.mock.mine(1);
    n.wait_settlement(height, SettlementState::Broadcast);
    n.mock.mine(1);
    n.wait_settlement(height, SettlementState::Confirmed);
}
