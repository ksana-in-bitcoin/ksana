//! M2 acceptance item 2: a path-B op submitted via RPC takes effect in the next block, and the tokens go into the
//! producer's locked balance.

mod common;

use std::time::Duration;

use common::*;
use ksana_node::settlement;
use ksana_primitives::{Entry, Payload, DEVNET};
use ksana_rpc::{codes, KsanaApiClient, OpState};

#[test]
fn path_b_op_via_rpc_lands_in_next_block_and_locks_fee_for_producer() {
    let dir = tempfile::tempdir().unwrap();
    let btc = TestBtc::new(dir.path());
    let (alice, bob, producer) = (user("alice"), user("bob"), user("producer"));
    // The genesis difficulty uses the PoW limit (block production speed is bounded by the minimum interval): an empty
    // block's difficulty is 12-120 times a non-empty block's (D-084, D-090), a genesis difficulty calibrated in seconds
    // would make empty blocks very slow
    let genesis = write_genesis(dir.path(), btc.anchor(), &[(&alice, 1_000_000)], None);
    let node = NodeProcess::start(&dir.path().join("data"), &genesis, Some(&producer), 2, &btc);
    let rt = rt();
    let c = node.client();
    wait_height(&rt, &c, 2, Duration::from_secs(120));

    // Rejections by policy and by consensus rules
    let low_fee = transfer_op(&alice, 0, 1, bob.account, 1);
    assert_eq!(
        error_code(&submit(&rt, &c, &low_fee).unwrap_err()),
        codes::OP_REJECTED
    );
    let wrong_chain = {
        let mut op = transfer_op(&alice, 0, 5, bob.account, 1);
        op.chain_id = 7;
        op
    };
    assert_eq!(
        error_code(&submit(&rt, &c, &wrong_chain).unwrap_err()),
        codes::OP_INVALID
    );

    // When submit returns, the op is already in the block template: the next block produced afterwards includes it
    let fee = 7u128;
    let op = transfer_op(&alice, 0, fee, bob.account, 100);
    let op_id = submit(&rt, &c, &op).unwrap();
    assert_eq!(op_id, op.op_id().unwrap());
    let tip_after_submit = latest(&rt, &c).header.height;
    let status = wait_included(&rt, &c, op_id, Duration::from_secs(120));
    assert_eq!(status.status, OpState::FastConfirmed, "{status:?}");
    let included = status.block.clone().unwrap();
    assert!(
        included.height == tip_after_submit || included.height == tip_after_submit + 1,
        "the op should take effect in the block after submission: tip after submit {tip_after_submit}, included at {}",
        included.height
    );
    let receipt = status.receipt.unwrap();
    assert!(receipt.success);
    assert_eq!(receipt.op_id, Some(op_id));

    // That block: it includes the op; its body is carried by the settlement transaction (paid by the producer's funding
    // UTXO) of itself or of some later block (D-077), and the body in the payload matches the block contents
    let block = block_at(&rt, &c, included.height);
    assert_eq!(block.hash, included.hash);
    assert!(
        matches!(&block.entries[status.index.unwrap() as usize], Entry::Sponsored { op: o } if *o == op)
    );
    let started = ksana_node::monotonic_now();
    let tx = loop {
        let s = rt
            .block_on(c.get_settlement(included.height.to_string()))
            .unwrap();
        if let Some(txs) = s.txs {
            assert_eq!(s.carrier.is_some(), block.settlement.is_none());
            break txs;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the body was not carried by a settlement transaction"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let payload = settlement::attached_payload(&DEVNET, &settlement::decode(&tx).unwrap()).unwrap();
    let payload = Payload::decode(&payload, &DEVNET).unwrap();
    let carried = ksana_btc::payload::bodies_in(&payload);
    let body = carried
        .iter()
        .find(|(hash, _)| *hash == block.hash)
        .expect("the payload carries that block's body");
    assert_eq!(body.1, ksana_btc::payload::entry_data(&block.entries));

    // Balances: the fee and the transfer take effect; the sponsored fee goes into the producer's locked records
    // (available balance unchanged)
    let a = account(&rt, &c, &alice);
    assert_eq!(ksa(&a), 1_000_000 - 100 - fee);
    assert_eq!(a.nonce, 1);
    assert_eq!(ksa(&account(&rt, &c, &bob)), 100);
    let p = account(&rt, &c, &producer);
    assert_eq!(
        ksa(&p),
        0,
        "the token fee and the block reward are locked and do not enter the available balance"
    );
    let locked = p
        .locked_fees
        .iter()
        .find(|l| l.block_hash == included.hash)
        .unwrap_or_else(|| {
            panic!(
                "the producer should have a locked record for that block: {:?}",
                p.locked_fees
            )
        });
    // The RPC lists that block's sponsored fee and the block reward to be paid out at unlock (D-074, D-079)
    assert_eq!(locked.amount, fee + DEVNET.block_reward);

    // After one more block it is still listed as usual (the sponsored fee is recorded at its acceptance position,
    // D-079)
    wait_height(&rt, &c, included.height + 1, Duration::from_secs(120));
    let p = account(&rt, &c, &producer);
    assert!(p
        .locked_fees
        .iter()
        .any(|l| l.block_hash == included.hash && l.amount == fee + DEVNET.block_reward));

    // Submitting again with the same nonce: nonce already used
    let replay = transfer_op(&alice, 0, 9, bob.account, 1);
    assert_eq!(
        error_code(&submit(&rt, &c, &replay).unwrap_err()),
        codes::OP_INVALID
    );

    // WebSocket subscription to new blocks
    let ws_url = node.rpc_url.replacen("http://", "ws://", 1);
    rt.block_on(async {
        let ws = jsonrpsee::ws_client::WsClientBuilder::default()
            .build(&ws_url)
            .await
            .unwrap();
        let mut sub = ws.subscribe_blocks().await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(120), sub.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(first.height > included.height);
        assert!(ws.is_connected());
    });
    node.stop();
}
