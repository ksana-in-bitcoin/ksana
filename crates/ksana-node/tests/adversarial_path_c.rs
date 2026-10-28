//! Adversarial test: end-to-end behavior on the node of the protocol binding of path-C ops (D-086) — copying someone
//! else's signed claim / transfer op verbatim into one's own BTC transaction.
//!
//! Both the copy and the original are native transactions and both are force-included (§7.4); the copy's signature is
//! bound to the original's first input, so signature verification fails in the copying transaction, it is recorded as
//! failed and does not consume the nonce; the original executes as usual. This holds whether the copy comes before or
//! after the original in the BTC block, and the beneficiary account receives only once.

mod common;

use bitcoin::hashes::Hash;
use bitcoin::{ScriptBuf, Sequence, Transaction, TxIn, Witness};
use common::mock_node::{native_tx, signed_native, wait, MockNode};
use common::user;
use ksana_primitives::Entry;
use ksana_rpc::BlockJson;

/// A transaction with exactly the same outputs as `original` (including the KSN1 data output carrying the op) that
/// spends a different output.
fn copy_of(n: &MockNode, original: &Transaction) -> Transaction {
    let input = n.mock.fund(&ksana_btc::mock::coinbase_script(), &[10_000])[0];
    Transaction {
        input: vec![TxIn {
            previous_output: input,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        ..original.clone()
    }
}

/// The first block on the main chain holding a forced-inclusion entry whose `btc_txid` is one of `txids`.
fn forced_block(n: &MockNode, txids: &[[u8; 32]]) -> BlockJson {
    for height in 1..=n.tip_height() {
        let block = n.block(height);
        if block
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Forced { btc_txid, .. } if txids.contains(btc_txid)))
        {
            return block;
        }
    }
    panic!("no block includes these native transactions");
}

fn forced_txids(block: &BlockJson) -> Vec<[u8; 32]> {
    block
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Forced { btc_txid, .. } => Some(*btc_txid),
            _ => None,
        })
        .collect()
}

#[test]
fn a_copied_native_op_fails_in_the_copying_transaction_and_only_the_original_pays() {
    let (carol, dave) = (user("adv/carol"), user("adv/dave"));
    let n = MockNode::start(&[(&carol, 100)]);
    wait("first block", || (n.tip_height() >= 1).then_some(()));

    // Round one: the copy comes before the original in the BTC block
    let (original, op) = native_tx(&n.mock, |input| {
        signed_native(&carol, 0, input, dave.account, 9)
    });
    let copy = copy_of(&n, &original);
    assert_eq!(copy.output, original.output);
    assert_ne!(copy.compute_txid(), original.compute_txid());
    n.mock
        .mine_block(vec![copy.clone(), original.clone()])
        .unwrap();
    let (copy_id, original_id) = (
        copy.compute_txid().to_byte_array(),
        original.compute_txid().to_byte_array(),
    );
    wait("dave receives 9 KSA", || (n.ksa(&dave) == 9).then_some(()));
    let block = forced_block(&n, &[copy_id, original_id]);
    assert_eq!(forced_txids(&block), vec![copy_id, original_id]);
    // Both entries carry the same op; signature verification fails for the copy and succeeds for the original
    for entry in &block.entries[..2] {
        assert!(matches!(entry, Entry::Forced { op: carried, .. } if *carried == op));
    }
    assert!(!block.receipts[0].success, "{:?}", block.receipts[0]);
    assert!(
        block.receipts[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("signature")),
        "{:?}",
        block.receipts[0]
    );
    assert!(block.receipts[1].success, "{:?}", block.receipts[1]);
    assert_eq!(n.ksa(&dave), 9);
    assert_eq!(n.ksa(&carol), 91);
    assert_eq!(
        n.account(&carol).nonce,
        1,
        "the copy does not consume the nonce"
    );

    // Round two: the original first and the copy after; the copy fails just the same
    let (original, op) = native_tx(&n.mock, |input| {
        signed_native(&carol, 1, input, dave.account, 4)
    });
    let copy = copy_of(&n, &original);
    n.mock
        .mine_block(vec![original.clone(), copy.clone()])
        .unwrap();
    let (copy_id, original_id) = (
        copy.compute_txid().to_byte_array(),
        original.compute_txid().to_byte_array(),
    );
    wait("dave receives 4 KSA", || (n.ksa(&dave) == 13).then_some(()));
    let block = forced_block(&n, &[copy_id, original_id]);
    assert_eq!(forced_txids(&block), vec![original_id, copy_id]);
    for entry in &block.entries[..2] {
        assert!(matches!(entry, Entry::Forced { op: carried, .. } if *carried == op));
    }
    assert!(block.receipts[0].success, "{:?}", block.receipts[0]);
    assert!(!block.receipts[1].success, "{:?}", block.receipts[1]);
    assert_eq!(n.ksa(&dave), 13);
    assert_eq!(n.ksa(&carol), 87);
    assert_eq!(n.account(&carol).nonce, 2);
    // Later blocks include neither of them again
    let height = block.header.height;
    wait("two more blocks", || {
        (n.tip_height() >= height + 2).then_some(())
    });
    for h in height + 1..=n.tip_height() {
        assert!(forced_txids(&n.block(h)).is_empty());
    }
}
