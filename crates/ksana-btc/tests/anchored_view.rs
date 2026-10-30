//! `confirmed_output` of the execution-layer view `AnchoredView` (the prevouts of the settlement transaction
//! carried by a block, D-034, D-035), on the mock chain:
//!
//! - Only gives transaction outputs (amount and script) confirmed on the anchored chain at a height not above the
//!   anchor height; transactions confirmed above the anchor height, only in the mempool, or unknown are all "absent".
//! - "Absent" is trustworthy only when txindex has synced up to the anchor height, otherwise it is a data source
//!   failure; an output that was found is not affected by txindex progress.
//! - The confirming block given by bitcoind is no longer on the best chain when its header is queried (a race
//!   between the query and a BTC reorg): it does not count as a confirmation on the anchored chain.
//! - The anchor block is not on the snapshot's best chain, is no longer on bitcoind's best chain after the query (BTC
//!   reorg), or bitcoind is unavailable: all are data source failures, never answered with "absent".
//! - The view used to sync historical blocks (`AnchoredView::history`, D-056): after the anchor block is reorged out
//!   of BTC it answers from the current best chain -- outputs are looked up by transaction ID at any height, and not
//!   finding one is a data source failure; headers are traced back along the invalidated branch (when bitcoind knows
//!   the invalidated anchor block), lower ones are taken from the current best chain.
//! - `native_outputs` (host function `native_tx`, D-078): all outputs of a native transaction, only from blocks on the
//!   anchored chain at a height not above the anchor height (native transactions in the snapshot, or outside the
//!   snapshot queried from bitcoind by transaction ID); not finding it is a data source failure. `native_input`
//!   (signature binding of a forced entry, D-086, D-100): the first input of the native transaction with that ID
//!   carrying that op, answered from the snapshot alone on the anchored chain (which holds every native transaction a
//!   forced entry can name), and "none" when no such transaction is there. When the anchor is invalid both answer
//!   from the current best chain, where not finding the transaction is a data source failure.
#![cfg(feature = "mock")]

use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::{
    absolute, transaction, Amount, Block, Script, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    WPubkeyHash, Witness,
};
use ksana_btc::client::{BtcClient, BtcError, HeaderInfo, ScannedUtxo, TxInfo, UtxoInfo};
use ksana_btc::data::data_script;
use ksana_btc::mock::MockBtc;
use ksana_btc::native::{native_op, outpoint_of, tx_outputs};
use ksana_btc::tracker::{
    AnchoredView, BtcSnapshot, BtcTracker, KnownBlocks, MainChain, MainEntry, TrackerError,
};
use ksana_execution::BtcView;
use ksana_primitives::{
    Action, BlockHeader, BtcAnchor, FeeMode, Payload, SigScheme, VmOp, DEVNET, KSA_ASSET_ID,
    VMOP_VERSION,
};

/// A main chain holding only a genesis block (these tests involve no fast-lane blocks).
fn genesis_main() -> Arc<MainChain> {
    Arc::new(MainChain::new(MainEntry {
        hash: [0; 32],
        settles: false,
        anchor_height: 0,
        producer: [0; 32],
        daa_score: 0,
        weight: 1,
        merged: Vec::new(),
    }))
}

/// There are no fast-lane settlement transactions on the chain of this test.
struct NoBlocks;

impl KnownBlocks for NoBlocks {
    fn count(&self, _snapshot: &BtcSnapshot, _header: &BlockHeader) -> Result<bool, TrackerError> {
        Ok(false)
    }
}

/// Forwards to the mock chain, and additionally simulates two bitcoind situations: txindex lagging behind the tip
/// (updated asynchronously after a new block is connected), and a transaction query answered from before a BTC reorg
/// (the given confirming block subsequently left the best chain).
struct Bitcoind {
    btc: Arc<MockBtc>,
    /// The height txindex has synced up to (`None` means not enabled).
    txindex: Option<u32>,
    /// The answer given when that transaction is queried.
    stale: Option<([u8; 32], TxInfo)>,
}

impl Bitcoind {
    fn lagging(btc: &Arc<MockBtc>, txindex: Option<u32>) -> Arc<dyn BtcClient> {
        Arc::new(Bitcoind {
            btc: Arc::clone(btc),
            txindex,
            stale: None,
        })
    }
}

impl BtcClient for Bitcoind {
    fn tip(&self) -> Result<(u32, [u8; 32]), BtcError> {
        self.btc.tip()
    }

    fn block_hash(&self, height: u32) -> Result<Option<[u8; 32]>, BtcError> {
        self.btc.block_hash(height)
    }

    fn block(&self, hash: &[u8; 32]) -> Result<Option<Block>, BtcError> {
        self.btc.block(hash)
    }

    fn header(&self, hash: &[u8; 32]) -> Result<Option<HeaderInfo>, BtcError> {
        self.btc.header(hash)
    }

    fn transaction(&self, txid: &[u8; 32]) -> Result<Option<TxInfo>, BtcError> {
        match &self.stale {
            Some((stale_txid, info)) if stale_txid == txid => Ok(Some(info.clone())),
            _ => self.btc.transaction(txid),
        }
    }

    fn utxo(
        &self,
        outpoint: &bitcoin::OutPoint,
        include_mempool: bool,
    ) -> Result<Option<UtxoInfo>, BtcError> {
        self.btc.utxo(outpoint, include_mempool)
    }

    fn scan_utxos(&self, script: &Script) -> Result<Vec<ScannedUtxo>, BtcError> {
        self.btc.scan_utxos(script)
    }

    fn test_accept(&self, txs: &[&Transaction]) -> Result<Result<(), String>, BtcError> {
        self.btc.test_accept(txs)
    }

    fn broadcast(&self, tx: &Transaction) -> Result<Result<(), String>, BtcError> {
        self.btc.broadcast(tx)
    }

    fn in_mempool(&self, txid: &[u8; 32]) -> Result<bool, BtcError> {
        self.btc.in_mempool(txid)
    }

    fn mempool_txids(&self) -> Result<Vec<[u8; 32]>, BtcError> {
        self.btc.mempool_txids()
    }

    fn txindex_height(&self) -> Result<Option<u32>, BtcError> {
        Ok(self.txindex)
    }
}

/// Mutually distinct P2WPKH locking scripts.
fn script(tag: u8) -> ScriptBuf {
    ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([tag; 20]))
}

/// A v2 transaction spending `inputs` and paying `outputs` (sats, script) (the mock chain does not check scripts or
/// signatures).
fn spend(inputs: &[bitcoin::OutPoint], outputs: &[(u64, &ScriptBuf)]) -> Transaction {
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

/// A snapshot synced up to bitcoind's current tip (the genesis anchor is the regtest genesis block).
fn snapshot(client: &Arc<dyn BtcClient>) -> Arc<BtcSnapshot> {
    let genesis = BlockHeader {
        version: 1,
        height: 0,
        parents: Vec::new(),
        btc_anchor: BtcAnchor {
            height: 0,
            hash: client.block_hash(0).unwrap().unwrap(),
        },
        entries_root: [0; 32],
        state_root: [0; 32],
        producer: [0; 32],
        timestamp: 1_700_000_000,
        bits: DEVNET.pow_limit_bits,
        nonce: 0,
    };
    let tracker = BtcTracker::new(Arc::clone(client), DEVNET, &genesis).unwrap();
    tracker.sync(&NoBlocks).unwrap();
    tracker.snapshot().unwrap()
}

/// A view anchored at height `height` on bitcoind's current best chain.
fn view_at(client: &Arc<dyn BtcClient>, snapshot: &Arc<BtcSnapshot>, height: u32) -> AnchoredView {
    let anchor = BtcAnchor {
        height,
        hash: client.block_hash(height).unwrap().unwrap(),
    };
    AnchoredView::new(
        anchor,
        Arc::clone(snapshot),
        Arc::clone(client),
        genesis_main(),
        DEVNET,
    )
}

/// Output number `vout` of transaction `txid`.
fn point(txid: [u8; 32], vout: u32) -> bitcoin::OutPoint {
    bitcoin::OutPoint {
        txid: bitcoin::Txid::from_byte_array(txid),
        vout,
    }
}

fn out(value: u64, script: &ScriptBuf) -> TxOut {
    TxOut {
        value: Amount::from_sat(value),
        script_pubkey: script.clone(),
    }
}

#[test]
fn outputs_confirmed_up_to_the_anchor() {
    let btc = Arc::new(MockBtc::new());
    let client: Arc<dyn BtcClient> = btc.clone();
    // Height 1: the funding transaction; height 2: spends its two outputs
    let funded = btc.fund(&script(1), &[50_000, 60_000]);
    let spent = spend(
        &[funded[1], funded[0]],
        &[(70_000, &script(2)), (39_000, &script(3))],
    );
    btc.submit(spent.clone()).unwrap();
    btc.mine(1);
    // A transaction only in the mempool
    let pending = spend(&[point(txid(&spent), 0)], &[(69_000, &script(4))]);
    btc.submit(pending.clone()).unwrap();
    let snap = snapshot(&client);

    let at2 = view_at(&client, &snap, 2);
    assert_eq!(
        at2.confirmed_output(&point(txid(&spent), 0)).unwrap(),
        Some(out(70_000, &script(2)))
    );
    assert_eq!(
        at2.confirmed_output(&point(txid(&spent), 1)).unwrap(),
        Some(out(39_000, &script(3)))
    );
    // An already spent output is still given (whether or not it is spent later); a nonexistent index is absent
    assert_eq!(
        at2.confirmed_output(&funded[1]).unwrap(),
        Some(out(60_000, &script(1)))
    );
    assert!(at2
        .confirmed_output(&point(txid(&spent), 2))
        .unwrap()
        .is_none());
    assert!(
        at2.confirmed_output(&point(txid(&pending), 0))
            .unwrap()
            .is_none(),
        "unconfirmed"
    );
    assert!(
        at2.confirmed_output(&point([0xab; 32], 0))
            .unwrap()
            .is_none(),
        "unknown"
    );

    // Anchored at height 1: the spending transaction is confirmed above the anchor height, the funding transaction is
    // still there
    let at1 = view_at(&client, &snap, 1);
    assert!(at1
        .confirmed_output(&point(txid(&spent), 0))
        .unwrap()
        .is_none());
    assert!(at1.confirmed_output(&funded[0]).unwrap().is_some());
}

#[test]
fn absent_is_trusted_only_when_txindex_has_reached_the_anchor() {
    let btc = Arc::new(MockBtc::new());
    let funded = btc.fund(&script(1), &[50_000]);
    let tx = spend(&[funded[0]], &[(49_000, &script(2))]);
    btc.submit(tx.clone()).unwrap();
    btc.mine(1);
    let lagging = Bitcoind::lagging(&btc, Some(1));
    let snap = snapshot(&lagging);
    let unknown = point([0xab; 32], 0);

    let view = view_at(&lagging, &snap, 2);
    // An output that was found does not depend on txindex progress; when not found, txindex lags behind the anchor
    // height and "absent" cannot be answered
    assert!(view
        .confirmed_output(&point(txid(&tx), 0))
        .unwrap()
        .is_some());
    assert!(view.confirmed_output(&unknown).is_err());
    // The view anchored at height 1: txindex has synced up to the anchor height, a transaction confirmed above the
    // anchor height is answered with "absent"
    assert!(view_at(&lagging, &snap, 1)
        .confirmed_output(&point(txid(&tx), 0))
        .unwrap()
        .is_none());
    // txindex not enabled
    let no_index = Bitcoind::lagging(&btc, None);
    assert!(view_at(&no_index, &snap, 2)
        .confirmed_output(&unknown)
        .is_err());
    // Once txindex has caught up with the anchor height, "absent" can be answered
    let synced = Bitcoind::lagging(&btc, Some(2));
    assert!(view_at(&synced, &snap, 2)
        .confirmed_output(&unknown)
        .unwrap()
        .is_none());
}

#[test]
fn confirmation_in_a_block_off_the_best_chain_does_not_count() {
    let btc = Arc::new(MockBtc::new());
    let funded = btc.fund(&script(1), &[50_000]);
    // Height 2 (old branch) confirms tx; after the reorg, height 2 of the new branch instead confirms rival, which
    // spends the same input, and tx is evicted
    let tx = spend(&[funded[0]], &[(49_000, &script(2))]);
    let rival = spend(&[funded[0]], &[(48_000, &script(3))]);
    btc.submit(tx.clone()).unwrap();
    let old = btc.mine(1)[0];
    btc.reorg(1);
    btc.mine_block(vec![rival.clone()]).unwrap();
    btc.mine(1);
    assert!(btc.transaction(&txid(&tx)).unwrap().is_none());

    // bitcoind's answer for tx comes from before the reorg: the confirming block is height 2 of the old branch
    let racing: Arc<dyn BtcClient> = Arc::new(Bitcoind {
        btc: btc.clone(),
        txindex: Some(3),
        stale: Some((
            txid(&tx),
            TxInfo {
                tx: tx.clone(),
                block: Some(old),
            },
        )),
    });
    let snap = snapshot(&racing);
    let view = view_at(&racing, &snap, 3);
    assert!(view
        .confirmed_output(&point(txid(&tx), 0))
        .unwrap()
        .is_none());
    assert_eq!(
        view.confirmed_output(&point(txid(&rival), 0)).unwrap(),
        Some(out(48_000, &script(3)))
    );
}

#[test]
fn anchor_off_the_best_chain_or_bitcoind_failure_is_an_error() {
    let btc = Arc::new(MockBtc::new());
    let client: Arc<dyn BtcClient> = btc.clone();
    let funded = btc.fund(&script(1), &[50_000]);
    let tx = spend(&[funded[0]], &[(49_000, &script(2))]);
    btc.submit(tx.clone()).unwrap();
    btc.mine(1);
    let snap = snapshot(&client);
    let view = view_at(&client, &snap, 2);
    let output = point(txid(&tx), 0);
    let unknown = point([0xab; 32], 0);
    assert!(view.confirmed_output(&output).unwrap().is_some());

    // bitcoind unavailable
    btc.set_offline(true);
    assert!(view.confirmed_output(&output).is_err());
    assert!(view.confirmed_output(&unknown).is_err());
    btc.set_offline(false);

    // BTC reorg: the transaction is reconfirmed at the same height on another branch, but the anchor block is no longer
    // on bitcoind's best chain
    btc.reorg(1);
    btc.mine(2);
    assert!(view.confirmed_output(&output).is_err());
    assert!(view.confirmed_output(&unknown).is_err());
    // The anchor block on the new branch is not in the old snapshot
    assert!(view_at(&client, &snap, 2)
        .confirmed_output(&output)
        .is_err());
    // New snapshot: anchored on the new branch it can be found
    let snap = snapshot(&client);
    assert!(view_at(&client, &snap, 2)
        .confirmed_output(&output)
        .unwrap()
        .is_some());
}

fn header_bytes(client: &Arc<dyn BtcClient>, hash: &[u8; 32]) -> [u8; 80] {
    let info = client.header(hash).unwrap().unwrap();
    bitcoin::consensus::encode::serialize(&info.header)
        .try_into()
        .unwrap()
}

#[test]
fn history_view_answers_from_the_current_chain_after_the_anchor_is_reorged_out() {
    let btc = Arc::new(MockBtc::new());
    let client: Arc<dyn BtcClient> = btc.clone();
    let funded = btc.fund(&script(1), &[50_000]);
    let tx = spend(&[funded[0]], &[(49_000, &script(2))]);
    btc.submit(tx.clone()).unwrap();
    btc.mine(1);
    // Anchor block X (height h) confirms tx; BTC reorgs X out, the new branch first mines an empty block, tx is
    // reconfirmed at height h + 1
    let (h, x) = btc.tip().unwrap();
    let x_header = header_bytes(&client, &x);
    let parent = header_bytes(&client, &client.block_hash(h - 1).unwrap().unwrap());
    btc.reorg(1);
    btc.mine_block(Vec::new()).unwrap();
    btc.mine(1);
    let snap = snapshot(&client);
    let anchor = BtcAnchor { height: h, hash: x };
    let none = genesis_main;

    // The view used for producing and receiving new blocks: the anchor block not being on the snapshot's best chain
    // is a data source failure
    let strict = AnchoredView::new(anchor.clone(), snap.clone(), client.clone(), none(), DEVNET);
    assert!(!strict.is_stale());
    assert!(strict.confirmed_output(&point(txid(&tx), 0)).is_err());

    let view = AnchoredView::history(anchor.clone(), snap.clone(), client.clone(), none(), DEVNET);
    assert!(view.is_stale());
    // Looked up by transaction ID on the current best chain at any height (h + 1 is above the anchor height)
    assert_eq!(
        view.confirmed_output(&point(txid(&tx), 0)).unwrap(),
        Some(out(49_000, &script(2)))
    );
    // A transaction not on the current best chain: data source failure, never answered with "absent"
    assert!(view.confirmed_output(&point([0xab; 32], 0)).is_err());
    // Headers: the anchor block itself is taken from the invalidated branch (bitcoind knows it), lower ones from the
    // current chain (they are ancestors of the anchor block), higher ones are absent
    assert_eq!(view.header(h).unwrap(), Some(x_header));
    assert_eq!(view.header(h - 1).unwrap(), Some(parent));
    assert_eq!(view.header(h + 1).unwrap(), None);
    // Settled closure: a block not on this chain has nothing to do with BTC, answered "no" as usual
    assert!(!view.block_settled(&[1; 32]).unwrap());

    // An anchor block bitcoind does not know: headers are answered from the current best chain
    let unknown = AnchoredView::history(
        BtcAnchor {
            height: h,
            hash: [0x77; 32],
        },
        snap.clone(),
        client.clone(),
        none(),
        DEVNET,
    );
    assert!(unknown.is_stale());
    let current = header_bytes(&client, &client.block_hash(h).unwrap().unwrap());
    assert_eq!(unknown.header(h).unwrap(), Some(current));

    // Anchored on the current best chain, or above the snapshot's tip: not an invalid anchor
    let tip = btc.tip().unwrap();
    let on_chain = BtcAnchor {
        height: tip.0,
        hash: tip.1,
    };
    assert!(
        !AnchoredView::history(on_chain, snap.clone(), client.clone(), none(), DEVNET).is_stale()
    );
    let above = BtcAnchor {
        height: tip.0 + 1,
        hash: [0x55; 32],
    };
    let above = AnchoredView::history(above, snap, client, none(), DEVNET);
    assert!(!above.is_stale());
    assert!(above.confirmed_output(&point(txid(&tx), 0)).is_err());
}

/// A native transaction spending `input`: a KSN1 data output (carrying one path C op) and a payment to `script(5)`.
fn native_tx(input: bitcoin::OutPoint) -> Transaction {
    let op = VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: [1; 32],
        nonce: 0,
        fee: FeeMode::BtcNative,
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to: [2; 32],
            amount: 1,
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    let payload = Payload::NativeOp { op }.encode(&DEVNET).unwrap();
    let data = data_script(&DEVNET.magic, &payload).unwrap();
    let mut tx = spend(&[input], &[(0, &data), (10_000, &script(5))]);
    tx.output.push(out(30_000, &script(6)));
    tx
}

#[test]
fn native_outputs_come_from_the_anchored_chain() {
    let btc = Arc::new(MockBtc::new());
    let client: Arc<dyn BtcClient> = btc.clone();
    let funded = btc.fund(&script(1), &[50_000, 60_000]);
    let native = native_tx(funded[0]);
    btc.submit(native.clone()).unwrap();
    btc.mine(1);
    // An ordinary transaction not in the snapshot's native transaction index (also queried from bitcoind by transaction
    // ID)
    let plain = spend(&[funded[1]], &[(59_000, &script(7))]);
    btc.submit(plain.clone()).unwrap();
    btc.mine(1);
    let (tip, _) = btc.tip().unwrap();
    let snap = snapshot(&client);
    assert!(
        snap.native_tx_in(&txid(&native), 0..=tip).is_some(),
        "the snapshot indexed the native transaction"
    );

    let at_tip = view_at(&client, &snap, tip);
    assert_eq!(
        at_tip.native_outputs(&txid(&native)).unwrap(),
        tx_outputs(&native)
    );
    assert_eq!(
        at_tip.native_outputs(&txid(&native)).unwrap()[1],
        (10_000, script(5).to_bytes())
    );
    assert_eq!(
        at_tip.native_outputs(&txid(&plain)).unwrap(),
        tx_outputs(&plain)
    );
    // The first input of the native transaction carrying the op (D-086, D-100)
    let op = native_op(&DEVNET, &native).unwrap();
    assert_eq!(
        at_tip.native_input(&txid(&native), &op).unwrap(),
        Some(outpoint_of(&funded[0]))
    );
    // No native transaction with this ID carries this op on the anchored chain: a transaction carrying no op, a
    // different op, a coinbase, an unknown transaction. The snapshot answers this alone (D-100)
    assert_eq!(at_tip.native_input(&txid(&plain), &op).unwrap(), None);
    let mut other = op.clone();
    other.nonce = 1;
    assert_eq!(at_tip.native_input(&txid(&native), &other).unwrap(), None);
    let first_hash = client.block_hash(1).unwrap().unwrap();
    let coinbase = txid(&client.block(&first_hash).unwrap().unwrap().txdata[0]);
    assert_eq!(at_tip.native_input(&coinbase, &op).unwrap(), None);
    assert_eq!(at_tip.native_input(&[0xab; 32], &op).unwrap(), None);
    // An unknown transaction, or one confirmed above the anchor height: the outputs are a data source failure (a
    // contract only asks for transactions it was shown), the input is "none"
    assert!(at_tip.native_outputs(&[0xab; 32]).is_err());
    let below = view_at(&client, &snap, tip - 2);
    assert!(below.native_outputs(&txid(&native)).is_err());
    assert!(below.native_outputs(&txid(&plain)).is_err());
    assert_eq!(below.native_input(&txid(&native), &op).unwrap(), None);

    // The anchor block is reorged out of BTC: the view for syncing historical blocks answers from the current best
    // chain (at any height)
    let anchor = BtcAnchor {
        height: tip,
        hash: btc.tip().unwrap().1,
    };
    btc.reorg(2);
    btc.mine_block(Vec::new()).unwrap();
    btc.mine(2);
    let snap = snapshot(&client);
    let none = genesis_main;
    let strict = AnchoredView::new(anchor.clone(), snap.clone(), client.clone(), none(), DEVNET);
    assert!(strict.native_outputs(&txid(&native)).is_err());
    // the anchor is not on the snapshot's best chain: a data source failure, never "none"
    assert!(strict.native_input(&txid(&native), &op).is_err());
    let history = AnchoredView::history(anchor, snap, client.clone(), none(), DEVNET);
    assert!(history.is_stale());
    assert_eq!(
        history.native_outputs(&txid(&native)).unwrap(),
        tx_outputs(&native)
    );
    assert_eq!(
        history.native_input(&txid(&native), &op).unwrap(),
        Some(outpoint_of(&funded[0]))
    );
    assert_eq!(history.native_input(&txid(&native), &other).unwrap(), None);
    assert!(history.native_outputs(&[0xab; 32]).is_err());
    assert!(history.native_input(&[0xab; 32], &op).is_err());
}

/// Pruning the snapshot at a pruning point (the cut the node and the indexer use: the pruning point's anchor height
/// less NATIVE_CONFIRMATIONS) never changes `native_input` on the anchor chain for a transaction a block still
/// processed can name: such a block's parents anchor at or above the pruning point's anchor, and its forced-inclusion
/// entries name transactions from its window start on (D-097, D-100).
#[test]
fn native_input_does_not_depend_on_pruning() {
    let btc = Arc::new(MockBtc::new());
    let client: Arc<dyn BtcClient> = btc.clone();
    let funded = btc.fund(&script(1), &[50_000, 60_000]);
    let native = native_tx(funded[0]);
    btc.submit(native.clone()).unwrap();
    btc.mine(1);
    let plain = spend(&[funded[1]], &[(59_000, &script(7))]);
    btc.submit(plain.clone()).unwrap();
    btc.mine(4);
    let (tip, _) = btc.tip().unwrap();
    let full = snapshot(&client);
    let native_height = (0..=tip)
        .find(|h| full.native_tx_in(&txid(&native), *h..=*h).is_some())
        .unwrap();
    let op = native_op(&DEVNET, &native).unwrap();
    let mut other = op.clone();
    other.nonce = 1;
    let asked = [
        (txid(&native), op.clone()),
        (txid(&native), other),
        (txid(&plain), op.clone()),
        ([0xab; 32], op),
    ];
    for point_anchor in 0..=tip {
        // the window start of a block whose parents anchor at the pruning point's anchor (devnet: h(x) = x)
        if point_anchor + 1 > native_height {
            break;
        }
        let mut pruned = BtcSnapshot::clone(&full);
        pruned.prune(
            point_anchor.saturating_sub(DEVNET.native_confirmations),
            &|_| false,
        );
        let pruned = Arc::new(pruned);
        for anchor in point_anchor..=tip {
            let (a, b) = (
                view_at(&client, &full, anchor),
                view_at(&client, &pruned, anchor),
            );
            for (txid, op) in &asked {
                assert_eq!(
                    a.native_input(txid, op),
                    b.native_input(txid, op),
                    "pruning point anchored at {point_anchor}, anchor at {anchor}"
                );
            }
        }
    }
}
