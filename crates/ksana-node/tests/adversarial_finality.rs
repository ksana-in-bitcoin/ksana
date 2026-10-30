//! Adversarial tests: behavior of BTC-anchored finality (D-082) in the face of an attacker, without the network
//! (several node instances share the same mock BTC chain, blocks are handed over manually).
//!
//! - **A BTC reorg deeper than `BTC_FINALITY_CONFS`** (the assumption boundary of SPEC §16): finalized blocks are
//!   revoked with the BTC reorg, the node falls back to the genesis block and switches to the chain that is heavier at
//!   that point; the attacker takes the chance to settle its own chain into the new BTC branch, and it becomes final
//!   first by reaching the confirmations first — all nodes consistently follow the chain BTC now carries, no node
//!   gets stuck, and ops on the revoked chain are dropped when invalid on the new main chain.
//! - **An attack block with only the header sent and the block withheld**: the attacker produces an empty block on top
//!   of the honest tip and sends only the header (along with the headers of the honest chain's empty blocks not yet
//!   landed) to BTC, broadcasting the block to nobody. Before that header reaches the confirmations, honest nodes judge
//!   earlier blocks final as usual; when it reaches them it is judged final (it precedes the honest successor blocks on
//!   BTC): honest nodes rebuild it from the header on BTC (D-093, an empty block's header is all of its data) and
//!   switch on top of it on the spot, even if their own later blocks are heavier together. Before D-093 a node would
//!   suspend the decision indefinitely for lack of this block and switch only when it got it (for a non-empty block
//!   with a withheld body see `m18_btc_rebuild.rs`).
//! - **The D-091 defense**: when an honest node sees a block on BTC heavier than its own landed part and lighter than
//!   its own tip, it republishes its own headers; as long as the honest chain is heavier and lands on BTC before the
//!   attack block reaches the confirmations, the attack block never becomes final and the decision never suspends.
//!   The republish pays the next-block estimate, whatever room bitcoind's mempool has (D-102, D-117).

mod common;

use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{Keypair, Secp256k1, SecretKey};
use bitcoin::{Amount, TxOut};
use common::side::{setup, Side};
use common::{transfer_op, user};
use ksana_btc::client::{BtcClient, MempoolInfo};
use ksana_btc::mock::MockBtc;
use ksana_btc::settlement::{self, ProducerUtxo};
use ksana_node::chain::Accepted;
use ksana_node::rescue::Rescuer;
use ksana_primitives::hash::sha256;
use ksana_primitives::{Block, BlockHeader, Payload, DEVNET};

/// Broadcasts the settlement transactions of blocks that have reached `SETTLEMENT_DEPTH` and gives them `confs`
/// confirmations on BTC.
fn settle(side: &Side, mock: &MockBtc, confs: u32) {
    side.sync();
    side.chain.process_settlements().unwrap();
    mock.mine(confs);
    side.sync();
}

fn txid(tx: &bitcoin::Transaction) -> [u8; 32] {
    tx.compute_txid().to_byte_array()
}

/// The fee rate (sat/vB, rounded down) the transactions in the BTC mempool pay together.
fn mempool_feerate(mock: &MockBtc) -> u64 {
    let txs = mock.mempool();
    let value = |outpoint: &bitcoin::OutPoint| match txs
        .iter()
        .find(|tx| tx.compute_txid() == outpoint.txid)
    {
        Some(tx) => tx.output[outpoint.vout as usize].value.to_sat(),
        None => mock
            .utxo(outpoint, false)
            .unwrap()
            .unwrap()
            .txout
            .value
            .to_sat(),
    };
    let fee: u64 = txs
        .iter()
        .map(|tx| {
            let input: u64 = tx.input.iter().map(|i| value(&i.previous_output)).sum();
            let output: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
            input - output
        })
        .sum();
    let vsize: u64 = txs.iter().map(|tx| tx.vsize() as u64).sum();
    fee / vsize
}

/// The attacker's own BTC funds: a key and several of its confirmed UTXOs (mines one BTC block).
struct Attacker {
    key: Keypair,
    utxos: Vec<ProducerUtxo>,
}

impl Attacker {
    fn fund(mock: &MockBtc, count: usize) -> Attacker {
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&sha256(b"adversarial/attacker/funding")).unwrap();
        let key = Keypair::from_secret_key(&secp, &secret);
        let script = settlement::producer_script(&key.x_only_public_key().0);
        let value = 200_000;
        let utxos = mock
            .fund(&script, &vec![value; count])
            .into_iter()
            .map(|outpoint| ProducerUtxo {
                outpoint,
                txout: TxOut {
                    value: Amount::from_sat(value),
                    script_pubkey: script.clone(),
                },
            })
            .collect();
        Attacker { key, utxos }
    }

    /// Puts the headers (without bodies) into a republish payload and sends it to the mock chain's mempool (anyone can
    /// send a republish payload).
    fn post_headers(&mut self, mock: &MockBtc, headers: Vec<BlockHeader>) {
        let payload = Payload::Headers {
            headers,
            bodies: vec![],
            payee: [0; 32],
        }
        .encode(&DEVNET)
        .unwrap();
        let utxo = self.utxos.pop().expect("the attacker ran out of UTXOs");
        let change = utxo.txout.script_pubkey.clone();
        let draft = settlement::draft(
            &DEVNET,
            &[utxo],
            change,
            &self.key.x_only_public_key().0,
            &payload,
            2,
        )
        .unwrap();
        let pair = settlement::finalize(&DEVNET, draft, &payload, &self.key).unwrap();
        mock.submit(pair.commit).unwrap();
        mock.submit(pair.reveal).unwrap();
    }
}

/// A's block 1 is final (its settlement has `BTC_FINALITY_CONFS` confirmations), B's heavier chain was never settled
/// and A treats it only as a side chain. Then BTC reorgs out the block holding the settlement and all the
/// confirmations after it, A's settlement transaction is held in the mempool and never lands again, and B settles its
/// own chain into the new branch and reaches the confirmations: A revokes the finality decision, falls back to the
/// genesis block and switches to B's chain by work; B's block 1 then becomes final on both nodes; A's former transfer
/// has its nonce already used on the new main chain and does not return to the entry pool; both nodes keep producing
/// on the same chain.
#[test]
fn a_btc_reorg_past_the_finality_confirmations_undoes_finality_and_nodes_follow_the_chain_btc_now_carries(
) {
    let (alice, bob, carol) = (user("adv/alice1"), user("adv/bob1"), user("adv/carol1"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let a = Side::new(&mock, &genesis, "adv-fin-a1");
    let b = Side::new(&mock, &genesis, "adv-fin-b1");
    let fork_height = mock.tip_height();
    let confs = DEVNET.btc_finality_confs;

    // Both chains anchor at BTC height fork_height: A's block 1 transfers to bob, then two more blocks so it reaches
    // SETTLEMENT_DEPTH; B transfers to carol with the same nonce on a heavier chain (B did not receive A's blocks)
    let op_to_bob = transfer_op(&alice, 0, 3, bob.account, 10);
    a.submit(op_to_bob.clone());
    let a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    b.submit(transfer_op(&alice, 0, 4, carol.account, 20));
    let b_blocks: Vec<Block> = (0..6).map(|_| b.mine()).collect();
    let (a1, b1, b6) = (
        a_blocks[0].header.hash(),
        b_blocks[0].header.hash(),
        b_blocks[5].header.hash(),
    );
    assert!(b.chain.tip().unwrap().chain_work > a.chain.tip().unwrap().chain_work);

    // A's settlement lands on BTC and reaches the confirmations: A's block 1 is final; B's chain is only a side chain
    // to A
    settle(&a, &mock, confs);
    let settled_at = fork_height + 1;
    assert_eq!(mock.tip_height(), settled_at + confs - 1);
    assert_eq!(a.chain.finalized().unwrap(), (a1, 1));
    for block in &b_blocks {
        assert!(matches!(a.accept(block).unwrap().outcome, Accepted::Side));
    }
    assert_eq!(a.chain.tip().unwrap().hash, a_blocks[2].header.hash());
    assert_eq!((a.ksa(&bob), a.ksa(&carol)), (10, 0));

    // BTC reorgs out the block holding the settlement and all the confirmations after it (depth = BTC_FINALITY_CONFS,
    // beyond the assumption of D-082); A's settlement transactions, back in the mempool, are held and never land again
    // (the attacker controls the packing of the new branch)
    let removed = mock.reorg(mock.tip_height() - fork_height);
    assert_eq!(removed.len() as u32, confs);
    assert_eq!(mock.tip_height(), fork_height);
    let returned = mock.mempool();
    assert_eq!(
        returned.len(),
        2,
        "A's commit and reveal transactions return to the mempool"
    );
    for tx in &returned {
        mock.hold(&txid(tx));
    }

    // A sees the reorg: the decision is revoked and it falls back to the genesis block; with no finalized block
    // constraining the main chain, it switches to B's chain by work
    a.sync();
    assert_eq!(a.chain.finalized().unwrap().1, 0);
    assert!(!a.chain.is_final(&a1, 1).unwrap());
    assert_eq!(a.chain.tip().unwrap().hash, b6);
    assert_eq!((a.ksa(&bob), a.ksa(&carol)), (0, 20));
    assert_eq!(a.chain.accepted_height(&a1).unwrap(), None);
    // The former transfer's nonce is already used on the new main chain, so it does not return to the entry pool
    assert!(!a.pending(&op_to_bob));

    // B settles its own chain into the new branch and reaches the confirmations: B's block 1 is final on both nodes
    settle(&b, &mock, confs);
    assert_eq!(b.chain.finalized().unwrap(), (b1, 1));
    a.sync();
    assert_eq!(a.chain.finalized().unwrap(), (b1, 1));
    assert!(a.chain.is_final(&b1, 1).unwrap());
    assert!(!a.chain.is_final(&a1, 1).unwrap());
    assert_eq!(a.chain.tip().unwrap().hash, b6);
    // A's held settlement transaction never landed: its block's data is not on BTC
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.counted_at(&a1), None);

    // B receives A's old chain: only a side chain; both nodes continue on the same chain
    for block in &a_blocks {
        assert!(matches!(b.accept(block).unwrap().outcome, Accepted::Side));
    }
    assert_eq!(b.chain.tip().unwrap().hash, b6);
    // A's new block has b6 as selected parent and merges its own discarded old tip (the transfer to bob in it has its
    // nonce already used and is skipped in lenient mode)
    let next = a.mine();
    assert!(next.header.parents.contains(&b6));
    assert!(next.header.parents.contains(&a_blocks[2].header.hash()));
    assert!(matches!(
        b.accept(&next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
    assert_eq!((a.ksa(&bob), a.ksa(&carol)), (0, 20));
    assert_eq!((b.ksa(&bob), b.ksa(&carol)), (0, 20));
}

/// Header sent, block withheld: A's block 1 (a transfer) lands on BTC with the settlement, the headers of the two
/// empty blocks after it have not landed yet. Attacker B produces an empty block X on top of A's tip, sends the
/// headers of a2, a3 and X to BTC together (anyone can send a republish payload; for X to become a candidate its whole
/// past must be on BTC) and does not broadcast X. A connects X into the header DAG: it is heavier than A's landed part
/// and its selected chain passes through the finalized block, so it becomes a candidate. Before X's header reaches
/// the confirmations, A's block 1 becomes final as usual, and A neither needs X nor rebuilds it; when it reaches them,
/// a2 and a3 (landed with X's header) become final, then it is X's turn: A does not have X, but an empty block's
/// header is all of its data, so in the same round of BTC sync A rebuilds X from the header (D-093), judges it final
/// and switches on top of X, dropping the blocks it produced in the meantime (their ops return to the entry pool) —
/// even though those blocks together are heavier than X: this is a situation the finality rule itself allows (the
/// honest chain's headers did not land in time), and the D-091 republish (the next test) is the defense against it.
#[test]
fn a_header_only_empty_block_is_rebuilt_from_btc_and_finalized_over_the_heavier_honest_chain() {
    let (alice, bob) = (user("adv/alice2"), user("adv/bob2"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let mut attacker = Attacker::fund(&mock, 2);
    let a = Side::new(&mock, &genesis, "adv-fin-a2");
    let b = Side::new(&mock, &genesis, "adv-fin-b2");
    let confs = DEVNET.btc_finality_confs;

    // A: block 1 transfers to bob, then two empty blocks so it reaches SETTLEMENT_DEPTH (settlement not broadcast yet)
    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    let a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    let (a1, a3) = (a_blocks[0].header.hash(), a_blocks[2].header.hash());

    // B: accepts A's chain, produces an empty block X on top of the tip and does not give it to A (produced before A's
    // settlement lands on BTC: no unlock entry, a truly empty block)
    for block in &a_blocks {
        b.accept(block).unwrap();
    }
    let x = b.mine();
    let x_hash = x.header.hash();
    assert_eq!(x.header.parents, vec![a3]);
    assert!(x.entries.is_empty() && x.settlement.is_none());

    // A's settlement lands on BTC with 1 confirmation — only block 1's header is on BTC; the attacker then sends the
    // headers of a2, a3 and X to BTC
    settle(&a, &mock, 1);
    let landed = mock.tip_height();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.counted_at(&a1), Some(landed));
    assert_eq!(snapshot.counted_at(&a3), None);
    attacker.post_headers(
        &mock,
        vec![
            a_blocks[1].header.clone(),
            a_blocks[2].header.clone(),
            x.header.clone(),
        ],
    );
    mock.mine(1);
    let x_seen = landed + 1;
    assert_eq!(mock.tip_height(), x_seen);

    // Before X's header reaches the confirmations: A's block 1 (data landed at landed) becomes final at
    // landed + confs − 1; X is connected but it is not its turn yet, and A does not need it
    mock.mine(confs - 2);
    a.sync();
    assert_eq!(mock.tip_height(), landed + confs - 1);
    assert_eq!(a.chain.finalized().unwrap(), (a1, 1));
    assert_eq!(
        a.chain.finality_evaluated_height().unwrap(),
        Some(landed + confs - 1)
    );
    assert!(a.chain.missing_parents().unwrap().is_empty());
    assert!(!a.chain.contains(&x_hash).unwrap());
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.counted_at(&a3), Some(x_seen));
    assert_eq!(snapshot.counted_at(&x_hash), Some(x_seen));

    // A, unaware of X, keeps producing on top of a3 (the first with a transfer and an unlock entry, the later empty
    // blocks with weights growing by streak) until heavier than X; none of these headers land on BTC
    let op_more = transfer_op(&alice, 1, 3, bob.account, 1);
    a.submit(op_more.clone());
    let mut more = vec![a.mine()];
    assert_eq!(more[0].header.parents, vec![a3]);
    while a.chain.tip().unwrap().chain_work <= b.chain.tip().unwrap().chain_work {
        assert!(more.len() < 8, "A's chain is slow to get heavier than X");
        more.push(a.mine());
    }
    assert_eq!(a.ksa(&bob), 11);

    // X's header reaches the confirmations: the candidate is X, a2 and a3 become final; when it is X's turn A does not
    // have it, rebuilds X from the header in the same sync round and judges it final, and the main chain must pass
    // through X: switch to X (even though it is lighter than A's own chain), A's later blocks are discarded, and
    // alice's second transfer returns to the entry pool
    mock.mine(1);
    a.sync();
    assert_eq!(mock.tip_height(), x_seen + confs - 1);
    assert_eq!(a.chain.finalized().unwrap(), (x_hash, 4));
    assert_eq!(
        a.chain.finality_evaluated_height().unwrap(),
        Some(mock.tip_height()),
        "the decision does not suspend"
    );
    assert!(a.chain.missing_parents().unwrap().is_empty());
    let rebuilt = a.chain.store().block(&x_hash).unwrap().unwrap();
    assert_eq!(rebuilt.header, x.header);
    assert!(rebuilt.entries.is_empty() && rebuilt.settlement.is_none());
    assert_eq!(a.chain.tip().unwrap().hash, x_hash);
    assert_eq!(a.ksa(&bob), 10);
    for block in &more {
        assert_eq!(a.chain.accepted_height(&block.header.hash()).unwrap(), None);
        assert!(!a
            .chain
            .is_final(&block.header.hash(), block.header.height)
            .unwrap());
    }
    assert!(a.pending(&op_more));
    // X delivered by B is only a duplicate
    assert!(matches!(a.accept(&x).unwrap().outcome, Accepted::Duplicate));
    // Then both nodes continue on top of X: A's new block has X as selected parent and merges its own discarded chain
    // (the transfer in it counts only once)
    let next = a.mine();
    assert!(next.header.parents.contains(&x_hash));
    assert_eq!(a.chain.tip().unwrap().hash, next.header.hash());
    assert_eq!(a.chain.accepted_height(&x_hash).unwrap(), Some(4));
    for block in &more {
        assert!(matches!(b.accept(block).unwrap().outcome, Accepted::Side));
    }
    assert!(matches!(
        b.accept(&next).unwrap().outcome,
        Accepted::Extended(_)
    ));
    b.sync();
    assert_eq!(a.chain.tip().unwrap(), b.chain.tip().unwrap());
    assert_eq!(b.chain.finalized().unwrap(), (x_hash, 4));
    assert_eq!((a.ksa(&bob), b.ksa(&bob)), (11, 11));
}

/// The same attack, but A runs the republish (D-091): after X's header lands on BTC, A has produced two more empty
/// blocks on top of a3 and its tip is heavier than X; after one BTC block A still has no carrier of its own, X becomes
/// the "competing chain" (heavier than A's part connected on BTC, lighter than A's tip), and A immediately republishes
/// the headers of a4 and a5. When X's header reaches the confirmations, the heaviest connected block is a5 and the
/// candidate chain does not pass through X: X never becomes final and A does not rebuild it; a4 and a5 become final
/// when the republish reaches the confirmations, and B rebuilds them from the republished headers and switches. The
/// republish races X, so it pays bitcoind's next-block estimate although the BTC mempool has room (D-091, D-117).
#[test]
fn republishing_outweighs_a_header_only_attack_block_before_it_can_become_final() {
    let (alice, bob) = (user("adv/alice3"), user("adv/bob3"));
    let (mock, genesis) = setup(&[(&alice, 1_000)]);
    let mut attacker = Attacker::fund(&mock, 2);
    let a = Side::new(&mock, &genesis, "adv-fin-a3");
    let b = Side::new(&mock, &genesis, "adv-fin-b3");
    let rescuer = Rescuer::new(
        Arc::clone(&a.chain),
        a.producer.funding.clone().unwrap(),
        Some(a.producer.producer),
        None,
    );
    let confs = DEVNET.btc_finality_confs;
    mock.set_fee_estimate(1, Some(20));
    mock.set_mempool_info(Some(MempoolInfo {
        vsize: 1_000,
        min_feerate: 1,
    }));

    a.submit(transfer_op(&alice, 0, 3, bob.account, 10));
    let a_blocks: Vec<Block> = (0..3).map(|_| a.mine()).collect();
    let (a1, a3) = (a_blocks[0].header.hash(), a_blocks[2].header.hash());

    for block in &a_blocks {
        b.accept(block).unwrap();
    }
    let x = b.mine();
    let x_hash = x.header.hash();
    assert!(x.entries.is_empty() && x.settlement.is_none());

    // A, unaware of X, produces two more empty blocks: weights grow by streak, and the tip is heavier than X (both
    // produced before the settlement lands on BTC: no unlock entry, no settlement transaction)
    let a4 = a.mine();
    let a5 = a.mine();
    assert_eq!(a4.header.parents, vec![a3]);
    assert!(a4.entries.is_empty() && a5.entries.is_empty());
    assert!(a.chain.tip().unwrap().chain_work > b.chain.tip().unwrap().chain_work);

    // A's settlement lands on BTC (only a1's header), and the attacker then sends the headers of a2, a3 and X to BTC
    settle(&a, &mock, 1);
    let landed = mock.tip_height();
    attacker.post_headers(
        &mock,
        vec![
            a_blocks[1].header.clone(),
            a_blocks[2].header.clone(),
            x.header.clone(),
        ],
    );
    mock.mine(1);
    let x_seen = landed + 1;
    // X's header has just appeared: not a competing chain yet, no republish
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.counted_at(&x_hash), Some(x_seen));
    assert_eq!(a.chain.btc_competitor(&snapshot, confs).unwrap(), None);
    rescuer.run_once().unwrap();
    assert!(mock.mempool().is_empty());
    // After one BTC block: X is a competing chain, and A republishes the headers of a4 and a5
    mock.mine(1);
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(
        a.chain.btc_competitor(&snapshot, confs).unwrap(),
        Some((x_hash, 4))
    );
    rescuer.run_once().unwrap();
    assert_eq!(
        mock.mempool().len(),
        2,
        "the republish's commit and reveal transactions"
    );
    assert_eq!(
        mempool_feerate(&mock),
        20,
        "the next-block estimate, not the floor"
    );
    mock.mine(1);
    let republished_at = x_seen + 2;
    a.sync();
    let snapshot = a.chain.btc().snapshot().unwrap();
    assert_eq!(snapshot.counted_at(&a4.header.hash()), Some(republished_at));
    assert_eq!(snapshot.counted_at(&a5.header.hash()), Some(republished_at));
    assert_eq!(a.chain.btc_competitor(&snapshot, confs).unwrap(), None);

    // X's header reaches the confirmations: the candidate is a5, a2 and a3 become final, X does not, and the decision
    // does not suspend
    mock.mine(confs - 3);
    a.sync();
    assert_eq!(mock.tip_height(), x_seen + confs - 1);
    assert_eq!(a.chain.finalized().unwrap(), (a3, 3));
    assert!(a.chain.is_final(&a1, 1).unwrap());
    assert_eq!(
        a.chain.finality_evaluated_height().unwrap(),
        Some(mock.tip_height())
    );
    assert!(a.chain.missing_parents().unwrap().is_empty());
    assert!(!a.chain.is_final(&x_hash, 4).unwrap());
    // The republish reaches the confirmations: a4 and a5 become final
    mock.mine(2);
    a.sync();
    assert_eq!(mock.tip_height(), republished_at + confs - 1);
    assert_eq!(a.chain.finalized().unwrap(), (a5.header.hash(), 5));
    assert!(!a.chain.is_final(&x_hash, 4).unwrap());
    assert_eq!(a.chain.tip().unwrap().hash, a5.header.hash());

    // B gets A's chain: heavier and final, so it switches over; X is discarded
    for block in [&a4, &a5] {
        b.accept(block).unwrap();
    }
    b.sync();
    assert_eq!(b.chain.tip().unwrap().hash, a5.header.hash());
    assert_eq!(b.chain.finalized().unwrap(), (a5.header.hash(), 5));
    assert_eq!(b.chain.accepted_height(&x_hash).unwrap(), None);
    // X obtained later by A is only a side chain
    assert!(matches!(a.accept(&x).unwrap().outcome, Accepted::Side));
    assert_eq!(a.chain.tip().unwrap().hash, a5.header.hash());
}
