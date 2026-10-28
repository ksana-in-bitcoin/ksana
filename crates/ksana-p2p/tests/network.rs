//! Multi-node network tests: the nodes all listen on 127.0.0.1 on system-assigned ports, and the Handler is an
//! in-memory chain (`support`).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ksana_p2p::{
    wire, ConnectionLimits, Handler, Keypair, Multiaddr, OpMessage, P2p, P2pConfig, PeerId,
    PeerLimits, Rate, Status, Tuning, Verdict,
};
use ksana_primitives::{Block, SettlementTxs};
use libp2p::futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::swarm::SwarmEvent;
use libp2p::{noise, tcp, yamux, StreamProtocol, SwarmBuilder};
use support::*;

const LONG: Duration = Duration::from_secs(15);

/// 1. Three nodes connected in a line (A–B, B–C, via bootstrap addresses): a block published by A reaches C
///    forwarded by B; an op published by C reaches A via B; a node's own published messages do not come back to its
///    own Handler.
#[test]
fn gossip_crosses_a_line_of_three_nodes() {
    let g = genesis(1);
    let a = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let b = TestNode::start(TestChain::new(g.clone()), vec![a.addr()], quiet_tuning());
    let c = TestNode::start(TestChain::new(g.clone()), vec![b.addr()], quiet_tuning());
    wait_gossip_ready(&a, &b);
    wait_gossip_ready(&b, &c);
    // Produce the block only after the first round after connecting (Status → entry pool) has finished, otherwise the
    // peer might get the block through sync instead of gossip.
    wait_mempool_served(&a, 1);
    wait_mempool_served(&b, 2);
    wait_mempool_served(&c, 1);
    assert!(!a.is_connected_to(&c));

    let block = a.chain.extend(1, 1).remove(0);
    let hash = block.header.hash();
    a.p2p.publish_block(&block);
    wait_until("C receives A's block", LONG, || c.chain.has_block(&hash));
    let at_b = b.chain.block_events();
    assert_eq!(at_b.len(), 1);
    assert!(at_b[0].gossip);
    assert_eq!(at_b[0].from, a.peer_id());
    assert_eq!(at_b[0].verdict, Verdict::Accept);
    let at_c = c.chain.block_events();
    assert_eq!(at_c.len(), 1);
    assert!(at_c[0].gossip);
    assert_eq!(at_c[0].from, b.peer_id());
    assert_eq!(at_c[0].hash, hash);
    assert_eq!(at_c[0].verdict, Verdict::Accept);

    let msg = op(1);
    c.p2p.publish_op(&msg);
    wait_until("A receives C's op", LONG, || {
        a.chain.op_events().iter().any(|e| e.msg == msg)
    });
    let at_a = a.chain.op_events();
    assert_eq!(at_a.len(), 1);
    assert_eq!(at_a[0].from, b.peer_id());
    assert_eq!(at_a[0].verdict, Verdict::Accept);
    assert_eq!(b.chain.op_events().len(), 1);

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        a.chain.block_events().is_empty(),
        "the block A published itself came back to A"
    );
    assert!(
        c.chain.op_events().is_empty(),
        "the op C published itself came back to C"
    );
    assert_eq!(c.chain.block_events().len(), 1);
}

/// 2. A late joiner: A has 1200 blocks; after connecting to A, C syncs all blocks in order over several GetBlocks
///    rounds, after which its Status equals A's.
#[test]
fn late_joiner_syncs_all_blocks_in_order() {
    let g = genesis(2);
    let a = TestNode::start(TestChain::new(g.clone()), vec![], Tuning::default());
    a.chain.extend(1200, 1);
    let c = TestNode::start(TestChain::new(g), vec![a.addr()], Tuning::default());
    wait_until("C syncs to 1200", Duration::from_secs(30), || {
        c.chain.tip_height() == 1200
    });
    assert_eq!(c.chain.status_now(), a.chain.status_now());
    assert_eq!(c.chain.main_chain(), a.chain.main_chain());
    let events = c.chain.block_events();
    let heights: Vec<u64> = events.iter().map(|e| e.height).collect();
    assert_eq!(heights, (1..=1200).collect::<Vec<u64>>());
    assert!(events
        .iter()
        .all(|e| !e.gossip && e.from == a.peer_id() && e.verdict == Verdict::Accept));
    // 500 + 500 + 200: at least three rounds.
    assert!(a.chain.blocks_after_calls() >= 3);
    assert!(a.chain.block_events().is_empty());
}

/// 3. A fork: C is on another, shorter branch; after connecting to A, which has more work, C's Handler receives A's
///    blocks through sync and switches over.
///    A has more work, but C's DAG tip (C's original branch) is one A lacks: A also obtains this branch from C (D-063:
///    it can be merged later).
#[test]
fn node_on_a_lighter_fork_receives_the_heavier_chain() {
    let g = genesis(3);
    let a = TestNode::start(TestChain::new(g.clone()), vec![], Tuning::default());
    a.chain.extend(30, 1);
    let c_chain = TestChain::new(g);
    let c_fork = c_chain.extend(5, 2);
    let c = TestNode::start(c_chain, vec![a.addr()], Tuning::default());
    wait_until("C switches to A's chain", LONG, || {
        c.chain.main_chain() == a.chain.main_chain()
    });
    assert_synced(&c.chain.block_events(), &a, &a.chain.main_chain()[1..]);
    // C's original branch is still stored, just not on the main chain
    assert!(c.chain.has_block(&c_fork[4].header.hash()));
    wait_until("A obtains C's branch", LONG, || {
        c_fork.iter().all(|b| a.chain.has_block(&b.header.hash()))
    });
    assert_eq!(
        a.chain.main_chain().len(),
        31,
        "A's main chain is unchanged"
    );
    let c_fork: Vec<_> = c_fork.iter().map(|b| b.header.hash()).collect();
    assert_synced(&a.chain.block_events(), &c, &c_fork);
    assert_eq!(a.chain.tips(), c.chain.tips());
}

/// Among the blocks received from `from` through sync (not gossip), exactly `expected` were accepted. When two nodes
/// sync with each other, one side's reply may contain blocks the other already has (e.g. A first obtains C's branch
/// and then answers C's GetBlocks); these duplicates are judged Ignore.
fn assert_synced(events: &[BlockEvent], from: &TestNode, expected: &[[u8; 32]]) {
    assert!(events.iter().all(|e| !e.gossip && e.from == from.peer_id()));
    let mut accepted: Vec<_> = events
        .iter()
        .filter(|e| e.verdict == Verdict::Accept)
        .map(|e| e.hash)
        .collect();
    accepted.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(accepted, expected, "every block is accepted exactly once");
    assert!(
        events
            .iter()
            .all(|e| e.verdict == Verdict::Accept || e.verdict == Verdict::Ignore),
        "{events:?}"
    );
}

/// DAG sync (D-063): the two nodes have the same main chain (same work), but C has an extra side block; A sees a tip
/// it lacks in C's Status and obtains it by syncing as usual.
#[test]
fn unknown_side_tips_are_synced_without_more_work() {
    let g = genesis(13);
    let a_chain = TestChain::new(g.clone());
    let c_chain = TestChain::new(g);
    for block in a_chain.extend(6, 1) {
        c_chain.add_block(block);
    }
    let side = child(&a_chain.block_at(3), 7);
    c_chain.add_block(side.clone());
    assert_eq!(a_chain.status_now().tip_work, c_chain.status_now().tip_work);
    let a = TestNode::start(a_chain, vec![], Tuning::default());
    let c = TestNode::start(c_chain, vec![a.addr()], Tuning::default());
    wait_until("A obtains C's side block", LONG, || {
        a.chain.has_block(&side.header.hash())
    });
    assert_eq!(a.chain.tips(), c.chain.tips());
    assert_eq!(a.chain.main_chain(), c.chain.main_chain());
}

/// When GetBlocks does not supply side blocks (e.g. the peer omitted old blocks), fetch by hash (D-063): first fetch
/// the peer's tip we lack, whose parent is unknown so it enters the orphan pool, then fetch the missing parents round
/// by round until the whole side chain is connected.
#[test]
fn side_branches_are_fetched_by_hash_when_get_blocks_omits_them() {
    let g = genesis(14);
    let a_chain = TestChain::new(g.clone());
    let c_chain = TestChain::new(g);
    for block in a_chain.extend(8, 1) {
        c_chain.add_block(block);
    }
    let mut parent = a_chain.block_at(2);
    let mut side = Vec::new();
    for salt in 0..3 {
        let block = child(&parent, 100 + salt);
        c_chain.add_block(block.clone());
        parent = block.clone();
        side.push(block);
    }
    c_chain.set_serve_side_blocks(false);
    let a = TestNode::start(a_chain, vec![], quiet_tuning());
    let c = TestNode::start(c_chain, vec![a.addr()], quiet_tuning());
    wait_until("A obtains C's side chain", LONG, || {
        side.iter().all(|b| a.chain.has_block(&b.header.hash()))
    });
    assert_eq!(a.chain.orphan_count(), 0);
    assert_eq!(a.chain.tips(), c.chain.tips());
    // The topmost block of the side chain arrives first (parent unknown), the rest are fetched one by one as missing
    // parents
    let events = a.chain.block_events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].hash, side[2].header.hash());
    assert_eq!(events[0].verdict, Verdict::MissingParent);
    assert!(events.iter().all(|e| !e.gossip && e.from == c.peer_id()));
}

/// 4. block_peer disconnects and stops gossip; after unblock_peer the bootstrap peer is redialed, the missed blocks
///    are caught up, and later blocks propagate as usual.
#[test]
fn block_peer_isolates_and_unblock_restores() {
    let g = genesis(4);
    let x = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let y = TestNode::start(TestChain::new(g), vec![x.addr()], quiet_tuning());
    wait_gossip_ready(&x, &y);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&y, 1);

    let b1 = x.chain.extend(1, 1).remove(0);
    x.p2p.publish_block(&b1);
    wait_until("Y receives b1", LONG, || {
        y.chain.has_block(&b1.header.hash())
    });

    x.p2p.block_peer(y.peer_id());
    wait_until("X disconnects Y", LONG, || !x.is_connected_to(&y));
    let b2 = x.chain.extend(1, 1).remove(0);
    let b2_hash = b2.header.hash();
    x.p2p.publish_block(&b2);
    // Y redials every 300 ms and is refused by X every time.
    holds_for(
        "no connection and no propagation while isolated",
        Duration::from_millis(1500),
        || !x.is_connected_to(&y) && !y.chain.has_block(&b2_hash),
    );

    let served = x.chain.mempool_calls();
    x.p2p.unblock_peer(y.peer_id());
    wait_until("Y reconnects and syncs to b2", LONG, || {
        y.chain.has_block(&b2_hash)
    });
    let b2_event = y
        .chain
        .block_events()
        .into_iter()
        .find(|e| e.hash == b2_hash)
        .unwrap();
    assert!(
        !b2_event.gossip,
        "b2 should come from the sync after reconnecting"
    );
    wait_gossip_ready(&x, &y);
    wait_mempool_served(&x, served + 1);

    let b3 = x.chain.extend(1, 1).remove(0);
    let b3_hash = b3.header.hash();
    x.p2p.publish_block(&b3);
    wait_until("Y receives b3", LONG, || y.chain.has_block(&b3_hash));
    let b3_event = y
        .chain
        .block_events()
        .into_iter()
        .find(|e| e.hash == b3_hash)
        .unwrap();
    assert!(b3_event.gossip);
    assert_eq!(y.chain.status_now(), x.chain.status_now());
}

/// 5. Genesis mismatch: both sides disconnect after exchanging Status, and X refuses Y's redials; the two exchange no
///    blocks, ops or entry pools.
#[test]
fn genesis_mismatch_disconnects_and_refuses() {
    let x = TestNode::start(TestChain::new(genesis(51)), vec![], quiet_tuning());
    x.chain.add_to_mempool(op(1));
    let y = TestNode::start(TestChain::new(genesis(52)), vec![x.addr()], quiet_tuning());
    y.chain.add_to_mempool(op(2));
    // Having answered the peer's Status request shows they were connected (the side that notices the mismatch first
    // may disconnect before answering).
    wait_until("Status exchanged", LONG, || {
        x.chain.status_calls() + y.chain.status_calls() >= 1
    });
    wait_until("disconnected", LONG, || {
        x.p2p.connected_peers().is_empty() && y.p2p.connected_peers().is_empty()
    });
    // Y redials every 300 ms (unless Y rejected X first), and X keeps refusing.
    let blocks = x.chain.extend(1, 1);
    x.p2p.publish_block(&blocks[0]);
    y.p2p.publish_op(&op(3));
    holds_for("X refuses Y", Duration::from_millis(1500), || {
        !x.is_connected_to(&y)
    });
    assert!(x.chain.block_events().is_empty() && y.chain.block_events().is_empty());
    assert!(x.chain.op_events().is_empty() && y.chain.op_events().is_empty());
    assert_eq!(x.chain.mempool_calls(), 0);
    assert_eq!(y.chain.mempool_calls(), 0);
    assert_eq!(y.chain.tip_height(), 0);
}

/// 6. A peer that keeps sending invalid blocks is disconnected and banned after the 3rd one: it cannot reconnect while
///    banned, can reconnect once the ban expires, and its score has been cleared.
#[test]
fn peer_sending_invalid_blocks_is_banned() {
    let g = genesis(6);
    let tuning = Tuning {
        ban_duration: Duration::from_secs(3),
        ..quiet_tuning()
    };
    let x = TestNode::start(TestChain::new(g.clone()), vec![], tuning.clone());
    let m = TestNode::start(TestChain::new(g.clone()), vec![x.addr()], tuning);
    wait_gossip_ready(&x, &m);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&m, 1);

    let rejected = || {
        x.chain
            .block_events()
            .iter()
            .filter(|e| matches!(e.verdict, Verdict::Reject(_)))
            .count()
    };
    for salt in 0..3 {
        m.p2p.publish_block(&bad_block(&g, salt));
    }
    wait_until("X disconnects M", LONG, || !x.is_connected_to(&m));
    assert_eq!(rejected(), 3);
    assert!(x.chain.block_events().iter().all(|e| e.from == m.peer_id()));
    // M redials every 300 ms and is refused throughout the ban.
    holds_for(
        "no reconnection while banned",
        Duration::from_millis(1800),
        || !x.is_connected_to(&m),
    );

    wait_until("reconnected after the ban expired", LONG, || {
        x.is_connected_to(&m)
    });
    wait_gossip_ready(&x, &m);
    m.p2p.publish_block(&bad_block(&g, 10));
    wait_until("X receives the 4th invalid block", LONG, || rejected() == 4);
    holds_for(
        "score cleared: 1 point does not ban",
        Duration::from_millis(800),
        || x.is_connected_to(&m),
    );
}

/// 7. After connecting, each side fetches the other's entry pool once, and the ops are handed to the Handler.
#[test]
fn mempool_is_fetched_on_connect() {
    let g = genesis(7);
    let x_chain = TestChain::new(g.clone());
    let x_ops: Vec<_> = (0..3).map(op).collect();
    for msg in &x_ops {
        x_chain.add_to_mempool(msg.clone());
    }
    let y_chain = TestChain::new(g);
    y_chain.add_to_mempool(op(100));
    let x = TestNode::start(x_chain, vec![], quiet_tuning());
    let y = TestNode::start(y_chain, vec![x.addr()], quiet_tuning());

    let y_op = op(100);
    // The order of the two entry pool requests is undetermined: the pool of the side that finishes first already has
    // the peer's ops, which get sent back (judged Ignore).
    wait_until("Y receives X's entry pool", LONG, || {
        let got: Vec<OpMessage> = y.chain.op_events().into_iter().map(|e| e.msg).collect();
        x_ops.iter().all(|m| got.contains(m))
    });
    let at_y = y.chain.op_events();
    let from_x: Vec<_> = at_y.iter().filter(|e| e.msg != y_op).collect();
    assert_eq!(
        from_x.iter().map(|e| e.msg.clone()).collect::<Vec<_>>(),
        x_ops
    );
    assert!(from_x
        .iter()
        .all(|e| e.from == x.peer_id() && e.verdict == Verdict::Accept));
    assert!(at_y
        .iter()
        .filter(|e| e.msg == y_op)
        .all(|e| e.verdict == Verdict::Ignore));
    wait_until("X receives Y's entry pool", LONG, || {
        x.chain.op_events().iter().any(|e| e.msg == y_op)
    });
    let at_x = x.chain.op_events();
    let event = at_x.iter().find(|e| e.msg == y_op).unwrap();
    assert_eq!(event.from, y.peer_id());
    assert_eq!(event.verdict, Verdict::Accept);
    assert!(at_x
        .iter()
        .filter(|e| e.msg != y_op)
        .all(|e| e.verdict == Verdict::Ignore));
}

/// When a gossip block's parent is unknown, sync from the source peer: Y missed several of X's blocks (produced while
/// isolated); a gossip block received afterwards has an unknown parent, and Y catches up from X right away (periodic
/// queries are off, so only MissingParent can trigger it).
#[test]
fn missing_parent_triggers_sync_from_the_source() {
    let g = genesis(8);
    let x = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let y = TestNode::start(TestChain::new(g), vec![x.addr()], quiet_tuning());
    wait_gossip_ready(&x, &y);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&y, 1);

    // Produce 5 blocks without broadcasting, then broadcast the 6th: its parent is unknown when Y receives it.
    x.chain.extend(5, 1);
    let b6 = x.chain.extend(1, 1).remove(0);
    x.p2p.publish_block(&b6);
    wait_until("Y catches up to 6 blocks", LONG, || {
        y.chain.tip_height() == 6
    });
    let events = y.chain.block_events();
    assert_eq!(events[0].verdict, Verdict::MissingParent);
    assert!(events[0].gossip);
    // Sync received 1..=6 (the gossiped b6 was judged parent-unknown and not stored).
    let synced: Vec<u64> = events[1..].iter().map(|e| e.height).collect();
    assert_eq!(synced, (1..=6).collect::<Vec<u64>>());
    assert!(events[1..].iter().all(|e| !e.gossip));
    assert_eq!(y.chain.status_now(), x.chain.status_now());
}

/// request_sync asks for Status and syncs right away (periodic queries are off).
#[test]
fn request_sync_pulls_missed_blocks() {
    let g = genesis(9);
    let x = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let y = TestNode::start(TestChain::new(g), vec![x.addr()], quiet_tuning());
    wait_gossip_ready(&x, &y);
    wait_mempool_served(&x, 1);
    x.chain.extend(7, 1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(y.chain.tip_height(), 0);
    y.p2p.request_sync();
    wait_until("Y syncs to 7", LONG, || y.chain.tip_height() == 7);
}

/// Periodic Status queries: missed blocks (not broadcast) are caught up at the next query.
#[test]
fn periodic_status_catches_up_without_gossip() {
    let g = genesis(10);
    let tuning = Tuning {
        status_interval: Duration::from_millis(300),
        ..quiet_tuning()
    };
    let x = TestNode::start(TestChain::new(g.clone()), vec![], tuning.clone());
    let y = TestNode::start(TestChain::new(g), vec![x.addr()], tuning);
    wait_gossip_ready(&x, &y);
    x.chain.extend(4, 1);
    wait_until("Y syncs to 4", LONG, || y.chain.tip_height() == 4);
    x.chain.extend(3, 1);
    wait_until("Y syncs to 7", LONG, || y.chain.tip_height() == 7);
}

/// A bootstrap address may be `/dns4/<hostname>/tcp/<port>` (without /p2p; devnet bootstraps by service name): dial by
/// address every time and remember the peer connected; a peer restarted on the same port with a new key (data
/// directory reset) can be reconnected to as well.
#[test]
fn dns_bootstrap_without_peer_id_redials_by_address() {
    let g = genesis(13);
    let x = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let port = x.port();
    let dns: libp2p::Multiaddr = format!("/dns4/localhost/tcp/{port}").parse().unwrap();
    let y = TestNode::start(TestChain::new(g.clone()), vec![dns], quiet_tuning());
    wait_gossip_ready(&x, &y);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&y, 1);
    let b1 = x.chain.extend(1, 1).remove(0);
    x.p2p.publish_block(&b1);
    wait_until("Y receives b1", LONG, || {
        y.chain.has_block(&b1.header.hash())
    });

    let old_peer = x.peer_id();
    x.p2p.stop();
    wait_until("Y sees the disconnect", LONG, || {
        y.p2p.connected_peers().is_empty()
    });
    let listen = format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap();
    let x2 = TestNode::start_on(TestChain::new(g), listen, vec![], quiet_tuning());
    assert_ne!(x2.peer_id(), old_peer);
    wait_until("Y connects to the restarted node by address", LONG, || {
        y.is_connected_to(&x2)
    });
}

/// Large messages: a 12 MiB block is delivered by gossip; a sync response close to the 32 MiB limit (3 blocks of
/// 10 MiB) is delivered as well.
#[test]
fn large_blocks_travel_by_gossip_and_sync() {
    let g = genesis(14);
    let x = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let y = TestNode::start(TestChain::new(g.clone()), vec![x.addr()], quiet_tuning());
    wait_gossip_ready(&x, &y);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&y, 1);

    let mut big = child(&g, 1);
    big.settlement = Some(SettlementTxs {
        commit: vec![0x41; 1024],
        reveal: vec![0x42; 12 * 1024 * 1024],
    });
    x.chain.add_local(big.clone());
    x.p2p.publish_block(&big);
    wait_until("Y receives the 12 MiB block by gossip", LONG, || {
        y.chain.has_block(&big.header.hash())
    });
    assert!(y.chain.block_events()[0].gossip);

    let mut parent = big;
    for salt in 0..3 {
        let mut next = child(&parent, salt);
        next.settlement = Some(SettlementTxs {
            commit: vec![salt as u8; 1024],
            reveal: vec![salt as u8; 10 * 1024 * 1024],
        });
        x.chain.add_local(next.clone());
        parent = next;
    }
    y.p2p.request_sync();
    wait_until("Y syncs to 4", LONG, || y.chain.tip_height() == 4);
    assert_eq!(y.chain.status_now(), x.chain.status_now());
}

/// stop closes the connections and returns; the peer sees the disconnect.
#[test]
fn stop_closes_connections_and_returns() {
    let g = genesis(11);
    let x = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let y = TestNode::start(TestChain::new(g), vec![x.addr()], quiet_tuning());
    wait_gossip_ready(&x, &y);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let TestNode { p2p, chain: _ } = y;
    std::thread::spawn(move || {
        p2p.stop();
        done_tx.send(()).unwrap();
    });
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("stop did not return");
    wait_until("X sees Y disconnect", LONG, || {
        x.p2p.connected_peers().is_empty()
    });
}

/// 8 (end to end): on the sync protocol, oversized frames, undecodable Borsh and trailing bytes are all rejected (not
/// handed to the Handler), the sender is penalized 1 point each time and disconnected after 3. The client is a libp2p
/// node that speaks only `/ksana/sync/2` and writes raw bytes directly.
#[test]
fn malformed_sync_requests_are_rejected_and_penalized() {
    let x = TestNode::start(TestChain::new(genesis(12)), vec![], quiet_tuning());
    let x_peer = x.peer_id();
    let x_addr = x.addr();
    let oversized = u32::try_from(wire::MAX_SYNC_MESSAGE_BYTES + 1).unwrap();
    let frames: Vec<Vec<u8>> = vec![
        // The declared length exceeds the limit: rejected right after reading the length, content not read.
        [oversized.to_le_bytes().to_vec(), vec![0u8; 16]].concat(),
        // Variant index 9 does not exist.
        [1u32.to_le_bytes().to_vec(), vec![9u8]].concat(),
        // One extra byte after Request::Status (full consumption is required).
        [2u32.to_le_bytes().to_vec(), vec![0u8, 0u8]].concat(),
    ];
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let client_peer = runtime.block_on(async move {
        let mut client = SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )
            .unwrap()
            .with_behaviour(|_| {
                request_response::Behaviour::with_codec(
                    RawCodec,
                    [(wire::SYNC_PROTOCOL, ProtocolSupport::Full)],
                    request_response::Config::default(),
                )
            })
            .unwrap()
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();
        let client_peer = *client.local_peer_id();
        client.dial(x_addr).unwrap();
        let outcome = tokio::time::timeout(LONG, async {
            let mut sent = false;
            loop {
                match client.select_next_some().await {
                    SwarmEvent::ConnectionEstablished { .. } if !sent => {
                        sent = true;
                        for frame in frames.clone() {
                            client.behaviour_mut().send_request(&x_peer, frame);
                        }
                    }
                    SwarmEvent::ConnectionClosed { .. } if sent => return,
                    _ => {}
                }
            }
        })
        .await;
        assert!(
            outcome.is_ok(),
            "X did not disconnect the peer sending invalid requests"
        );
        client_peer
    });
    // The client may see the connection close before X's event loop does.
    wait_until("X disconnects the peer", LONG, || {
        !x.p2p.connected_peers().contains(&client_peer)
    });
    holds_for("X refuses the peer", Duration::from_millis(300), || {
        !x.p2p.connected_peers().contains(&client_peer)
    });
    // No request reached the Handler.
    assert_eq!(x.chain.status_calls(), 0);
    assert_eq!(x.chain.blocks_after_calls(), 0);
    assert_eq!(x.chain.mempool_calls(), 0);
}

/// Rate limiting (D-059): gossip ops and blocks a peer sends beyond its quota are dropped before decoding (not handed
/// to the Handler, no score, no disconnect), and are processed as usual once the tokens refill.
#[test]
fn gossip_beyond_the_peer_quota_is_dropped_without_penalty() {
    let g = genesis(15);
    let tight = Tuning {
        peer_limits: PeerLimits {
            ops: Rate {
                per_sec: 1,
                burst: 5,
            },
            blocks: Rate {
                per_sec: 1,
                burst: 2,
            },
            ..PeerLimits::default()
        },
        ..quiet_tuning()
    };
    let x = TestNode::start(TestChain::new(g.clone()), vec![], tight);
    let m = TestNode::start(TestChain::new(g), vec![x.addr()], quiet_tuning());
    wait_gossip_ready(&x, &m);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&m, 1);

    for nonce in 0..40 {
        m.p2p.publish_op(&op(nonce));
    }
    let blocks = m.chain.extend(6, 1);
    for block in &blocks {
        m.p2p.publish_block(block);
    }
    wait_until(
        "X receives the ops and blocks within the quota",
        LONG,
        || x.chain.op_events().len() >= 5 && x.chain.tip_height() >= 2,
    );
    holds_for(
        "those beyond the quota are dropped, M is not disconnected",
        Duration::from_millis(800),
        || x.is_connected_to(&m),
    );
    let ops = x.chain.op_events();
    assert!((5..=6).contains(&ops.len()), "{} ops received", ops.len());
    assert!(ops
        .iter()
        .all(|e| e.verdict == Verdict::Accept && e.from == m.peer_id()));
    let block_events = x.chain.block_events();
    assert!(
        (2..=3).contains(&block_events.len()),
        "{} blocks received",
        block_events.len()
    );
    assert!(block_events
        .iter()
        .all(|e| e.gossip && e.verdict == Verdict::Accept));

    // Tokens refill at 1 per second: later ops are processed as usual
    std::thread::sleep(Duration::from_millis(1_200));
    let later = op(1_000);
    m.p2p.publish_op(&later);
    wait_until("a new op is received after the tokens refill", LONG, || {
        x.chain.op_events().iter().any(|e| e.msg == later)
    });
    assert!(x.is_connected_to(&m));
}

/// Rate limiting (D-059): sync requests beyond the quota are dropped (the peer sees a failure), not handed to the
/// Handler, no score, no disconnect; those within the quota are answered as usual.
/// The client is a libp2p node writing raw bytes directly, sending 20 valid `Status` requests at once.
#[test]
fn sync_requests_beyond_the_peer_quota_are_dropped_without_penalty() {
    let tight = Tuning {
        peer_limits: PeerLimits {
            requests: Rate {
                per_sec: 1,
                burst: 5,
            },
            ..PeerLimits::default()
        },
        ..quiet_tuning()
    };
    let x = TestNode::start(TestChain::new(genesis(16)), vec![], tight);
    let x_peer = x.peer_id();
    let x_addr = x.addr();
    // Request::Status: 4-byte little-endian length 1, variant index 0
    let status = [1u32.to_le_bytes().to_vec(), vec![0u8]].concat();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (client_peer, answered, client) = runtime.block_on(async move {
        let mut client = SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )
            .unwrap()
            .with_behaviour(|_| {
                request_response::Behaviour::with_codec(
                    RawCodec,
                    [(wire::SYNC_PROTOCOL, ProtocolSupport::Full)],
                    request_response::Config::default(),
                )
            })
            .unwrap()
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();
        let client_peer = *client.local_peer_id();
        client.dial(x_addr).unwrap();
        let mut sent = false;
        let (mut answered, mut finished) = (0usize, 0usize);
        let outcome = tokio::time::timeout(LONG, async {
            loop {
                match client.select_next_some().await {
                    SwarmEvent::ConnectionEstablished { .. } if !sent => {
                        sent = true;
                        for _ in 0..20 {
                            client.behaviour_mut().send_request(&x_peer, status.clone());
                        }
                    }
                    SwarmEvent::Behaviour(request_response::Event::Message {
                        message: request_response::Message::Response { response, .. },
                        ..
                    }) => {
                        // A dropped request may read an empty response (the peer closed the stream outright)
                        if !response.is_empty() {
                            answered += 1;
                        }
                        finished += 1;
                    }
                    SwarmEvent::Behaviour(request_response::Event::OutboundFailure { .. }) => {
                        finished += 1;
                    }
                    _ => {}
                }
                if finished == 20 {
                    return;
                }
            }
        })
        .await;
        assert!(outcome.is_ok(), "not all 20 requests finished");
        (client_peer, answered, client)
    });
    assert!((5..=6).contains(&answered), "{answered} requests answered");
    assert_eq!(x.chain.status_calls(), answered);
    holds_for(
        "no score, no disconnect",
        Duration::from_millis(500),
        || x.p2p.connected_peers().contains(&client_peer),
    );
    drop(client);
}

/// A codec that reads and writes raw bytes directly (only for constructing invalid requests).
#[derive(Clone, Copy, Default)]
struct RawCodec;

impl request_response::Codec for RawCodec {
    type Protocol = StreamProtocol;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(&mut self, _: &StreamProtocol, io: &mut T) -> std::io::Result<Vec<u8>>
    where
        T: libp2p::futures::AsyncRead + Unpin + Send,
    {
        let mut buf = Vec::new();
        io.take(1 << 20).read_to_end(&mut buf).await?;
        Ok(buf)
    }

    async fn read_response<T>(&mut self, _: &StreamProtocol, io: &mut T) -> std::io::Result<Vec<u8>>
    where
        T: libp2p::futures::AsyncRead + Unpin + Send,
    {
        let mut buf = Vec::new();
        io.take(1 << 20).read_to_end(&mut buf).await?;
        Ok(buf)
    }

    async fn write_request<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        request: Vec<u8>,
    ) -> std::io::Result<()>
    where
        T: libp2p::futures::AsyncWrite + Unpin + Send,
    {
        io.write_all(&request).await
    }

    async fn write_response<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        response: Vec<u8>,
    ) -> std::io::Result<()>
    where
        T: libp2p::futures::AsyncWrite + Unpin + Send,
    {
        io.write_all(&response).await
    }
}

/// A Handler that takes `delay` over every block, so that gossip piles up in the validation queue.
struct SlowBlocks {
    chain: Arc<TestChain>,
    delay: Duration,
}

impl Handler for SlowBlocks {
    fn status(&self) -> Status {
        self.chain.status()
    }

    fn locator(&self) -> Vec<[u8; 32]> {
        self.chain.locator()
    }

    fn blocks_after(&self, locator: &[[u8; 32]], after: Option<&[u8; 32]>, max: u32) -> Vec<Block> {
        self.chain.blocks_after(locator, after, max)
    }

    fn block(&self, hash: &[u8; 32]) -> Option<Block> {
        self.chain.block(hash)
    }

    fn unknown(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        self.chain.unknown(hashes)
    }

    fn missing_parents(&self) -> Vec<[u8; 32]> {
        self.chain.missing_parents()
    }

    fn mempool(&self, max: u32) -> Vec<OpMessage> {
        self.chain.mempool(max)
    }

    fn on_block(&self, from: PeerId, block: Block, gossip: bool) -> Verdict {
        std::thread::sleep(self.delay);
        self.chain.on_block(from, block, gossip)
    }

    fn on_op(&self, from: PeerId, msg: OpMessage) -> Verdict {
        self.chain.on_op(from, msg)
    }
}

/// The audit finding (M26): gossip is validated one message at a time in arrival order, so the messages a peer had
/// queued before it was banned were still decoded and handed to the Handler afterwards, each costing as much as a
/// valid one. Once a peer is banned its queued messages are dropped without validation.
#[test]
fn queued_gossip_from_a_banned_peer_is_not_validated() {
    const DELAY: Duration = Duration::from_millis(300);
    let g = genesis(17);
    let chain = TestChain::new(g.clone());
    let config = P2pConfig {
        keypair: Keypair::generate_ed25519(),
        listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
        peers: vec![],
        genesis: chain.genesis_hash(),
        max_op_bytes: ksana_primitives::params::DEVNET.max_op_bytes,
    };
    let slow = SlowBlocks {
        chain: chain.clone(),
        delay: DELAY,
    };
    let x = TestNode {
        p2p: P2p::start_with(config, quiet_tuning(), Arc::new(slow)).unwrap(),
        chain,
    };
    let m = TestNode::start(TestChain::new(g.clone()), vec![x.addr()], quiet_tuning());
    wait_gossip_ready(&x, &m);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&m, 1);

    const SENT: u64 = 10;
    for salt in 0..SENT {
        m.p2p.publish_block(&bad_block(&g, salt));
    }
    wait_until("X bans M", LONG, || !x.is_connected_to(&m));
    // Time enough for X to validate everything M sent
    std::thread::sleep(DELAY * SENT as u32);
    let validated = x.chain.block_events().len();
    eprintln!("{validated} of the {SENT} invalid blocks M sent were validated");
    assert!(x
        .chain
        .block_events()
        .iter()
        .all(|e| e.from == m.peer_id() && matches!(e.verdict, Verdict::Reject(_))));
    // The third invalid block bans M; at most the one taken from the queue meanwhile is validated after it
    assert!(
        (3..=4).contains(&validated),
        "{validated} of the {SENT} invalid blocks M sent were validated"
    );
}

/// Connection limits (D-132): a node takes at most `max_inbound` inbound connections; a peer over it is refused until
/// a place frees up.
#[test]
fn inbound_connections_beyond_the_limit_are_refused() {
    let g = genesis(18);
    let one = Tuning {
        connections: ConnectionLimits {
            max_inbound: 1,
            ..ConnectionLimits::default()
        },
        ..quiet_tuning()
    };
    let x = TestNode::start(TestChain::new(g.clone()), vec![], one);
    let a = TestNode::start(TestChain::new(g.clone()), vec![x.addr()], quiet_tuning());
    wait_until("X takes A", LONG, || x.is_connected_to(&a));
    let b = TestNode::start(TestChain::new(g), vec![x.addr()], quiet_tuning());
    holds_for(
        "X refuses B while A holds the place",
        Duration::from_millis(1_000),
        || !x.is_connected_to(&b) && x.p2p.connected_peers().len() == 1,
    );
    drop(a);
    wait_until("X takes B once A is gone", LONG, || x.is_connected_to(&b));
}

/// D-132: no op longer than MAX_OP_BYTES can be valid, so such an op is rejected before it is decoded (it never
/// reaches the Handler) and costs its sender score, while an op exactly MAX_OP_BYTES long is handled as usual. The
/// sender here believes in a larger limit.
#[test]
fn gossip_ops_longer_than_max_op_bytes_are_rejected_before_decoding() {
    let g = genesis(19);
    let max = ksana_primitives::params::DEVNET.max_op_bytes;
    let x = TestNode::start(TestChain::new(g.clone()), vec![], quiet_tuning());
    let m = TestNode::start_with_op_limit(
        TestChain::new(g),
        "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
        vec![x.addr()],
        quiet_tuning(),
        4 * max,
    );
    wait_gossip_ready(&x, &m);
    wait_mempool_served(&x, 1);
    wait_mempool_served(&m, 1);

    let longest = op_of_len(1, max);
    m.p2p.publish_op(&longest);
    wait_until("X takes an op exactly MAX_OP_BYTES long", LONG, || {
        x.chain.op_events().iter().any(|e| e.msg == longest)
    });
    // Three ops one byte longer: none reaches the Handler, and the third brings M to the ban threshold
    for nonce in 2..5 {
        m.p2p.publish_op(&op_of_len(nonce, max + 1));
    }
    wait_until("X bans M", LONG, || !x.is_connected_to(&m));
    assert_eq!(x.chain.op_events().len(), 1);
}

/// D-132: the Handler serves inbound sync requests at most four at a time; the others wait their turn and are answered
/// all the same.
#[test]
fn inbound_requests_are_served_a_few_at_a_time() {
    let chain = TestChain::new(genesis(20));
    let slow = Arc::new(SlowStatus {
        chain: chain.clone(),
        delay: Duration::from_millis(300),
        busy: AtomicUsize::new(0),
        most: AtomicUsize::new(0),
    });
    let config = P2pConfig {
        keypair: Keypair::generate_ed25519(),
        listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
        peers: vec![],
        genesis: chain.genesis_hash(),
        max_op_bytes: ksana_primitives::params::DEVNET.max_op_bytes,
    };
    let x = TestNode {
        p2p: P2p::start_with(config, quiet_tuning(), slow.clone()).unwrap(),
        chain,
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // Six requests at once: with this node's own Status request to the client they take seven of the eight streams a
    // connection may have open
    let answered = runtime.block_on(ask_status(
        Keypair::generate_ed25519(),
        x.peer_id(),
        x.addr(),
        6,
    ));
    assert_eq!(answered, 6);
    let most = slow.most.load(Ordering::SeqCst);
    assert!((1..=4).contains(&most), "{most} requests served at once");
}

/// D-132: the token buckets are the source's and outlive its connections: a peer that used up its request quota does
/// not get a new one by reconnecting.
#[test]
fn request_quota_is_not_renewed_by_reconnecting() {
    let tight = Tuning {
        peer_limits: PeerLimits {
            requests: Rate {
                per_sec: 1,
                burst: 5,
            },
            ..PeerLimits::default()
        },
        ..quiet_tuning()
    };
    let x = TestNode::start(TestChain::new(genesis(21)), vec![], tight);
    let identity = Keypair::generate_ed25519();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let first = runtime.block_on(ask_status(identity.clone(), x.peer_id(), x.addr(), 5));
    assert_eq!(first, 5);
    wait_until("X sees the client leave", LONG, || {
        x.p2p.connected_peers().is_empty()
    });
    let again = runtime.block_on(ask_status(identity, x.peer_id(), x.addr(), 5));
    // At most what refilled at one token a second meanwhile
    assert!(again <= 2, "{again} requests answered after reconnecting");
}

/// A libp2p node with `identity` writing raw bytes directly.
fn raw_client(identity: Keypair) -> libp2p::Swarm<request_response::Behaviour<RawCodec>> {
    SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .unwrap()
        .with_behaviour(|_| {
            request_response::Behaviour::with_codec(
                RawCodec,
                [(wire::SYNC_PROTOCOL, ProtocolSupport::Full)],
                request_response::Config::default(),
            )
        })
        .unwrap()
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build()
}

/// Connect to `peer` at `addr` as `identity`, send `count` `Status` requests at once, and count those answered; the
/// connection is closed on return.
async fn ask_status(identity: Keypair, peer: PeerId, addr: Multiaddr, count: usize) -> usize {
    // Request::Status: 4-byte little-endian length 1, variant index 0
    let status = [1u32.to_le_bytes().to_vec(), vec![0u8]].concat();
    let mut client = raw_client(identity);
    client.dial(addr).unwrap();
    let (mut sent, mut answered, mut finished) = (false, 0, 0);
    let outcome = tokio::time::timeout(LONG, async {
        while finished < count {
            match client.select_next_some().await {
                SwarmEvent::ConnectionEstablished { .. } if !sent => {
                    sent = true;
                    for _ in 0..count {
                        client.behaviour_mut().send_request(&peer, status.clone());
                    }
                }
                SwarmEvent::Behaviour(request_response::Event::Message {
                    message: request_response::Message::Response { response, .. },
                    ..
                }) => {
                    // A dropped request may read an empty response (the peer closed the stream outright)
                    if !response.is_empty() {
                        answered += 1;
                    }
                    finished += 1;
                }
                SwarmEvent::Behaviour(request_response::Event::OutboundFailure { .. }) => {
                    finished += 1;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(outcome.is_ok(), "not all {count} requests finished");
    answered
}

/// A Handler that takes `delay` over every `Status` and records how many it serves at once.
struct SlowStatus {
    chain: Arc<TestChain>,
    delay: Duration,
    busy: AtomicUsize,
    most: AtomicUsize,
}

impl Handler for SlowStatus {
    fn status(&self) -> Status {
        let busy = self.busy.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(busy, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        self.busy.fetch_sub(1, Ordering::SeqCst);
        self.chain.status()
    }

    fn locator(&self) -> Vec<[u8; 32]> {
        self.chain.locator()
    }

    fn blocks_after(&self, locator: &[[u8; 32]], after: Option<&[u8; 32]>, max: u32) -> Vec<Block> {
        self.chain.blocks_after(locator, after, max)
    }

    fn block(&self, hash: &[u8; 32]) -> Option<Block> {
        self.chain.block(hash)
    }

    fn unknown(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        self.chain.unknown(hashes)
    }

    fn missing_parents(&self) -> Vec<[u8; 32]> {
        self.chain.missing_parents()
    }

    fn mempool(&self, max: u32) -> Vec<OpMessage> {
        self.chain.mempool(max)
    }

    fn on_block(&self, from: PeerId, block: Block, gossip: bool) -> Verdict {
        self.chain.on_block(from, block, gossip)
    }

    fn on_op(&self, from: PeerId, msg: OpMessage) -> Verdict {
        self.chain.on_op(from, msg)
    }
}
