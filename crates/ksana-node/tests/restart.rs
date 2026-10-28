//! M2 acceptance item 3: after a restart the node recovers from storage with consistent state (both a normal stop and
//! a crash).

mod common;

use std::time::Duration;

use common::*;
use ksana_node::chain::Chain;
use ksana_node::genesis::GenesisFile;
use ksana_primitives::DEVNET;
use ksana_rpc::{KsanaApiClient, OpState};

#[test]
fn node_recovers_the_same_state_after_restart_and_crash() {
    let dir = tempfile::tempdir().unwrap();
    let btc = TestBtc::new(dir.path());
    let data = dir.path().join("data");
    let (alice, bob, producer) = (user("alice"), user("bob"), user("producer"));
    // The genesis difficulty uses the PoW limit (block production speed is bounded by the minimum interval): an empty
    // block's difficulty is 12-120 times a non-empty block's (D-084, D-090), a genesis difficulty calibrated in seconds
    // would make empty blocks very slow
    let genesis = write_genesis(
        dir.path(),
        btc.anchor(),
        &[(&alice, 50_000), (&bob, 50_000)],
        None,
    );
    let rt = rt();

    // 1. Produce blocks and include several path-B ops
    let node = NodeProcess::start(&data, &genesis, Some(&producer), 1, &btc);
    let c = node.client();
    wait_height(&rt, &c, 2, Duration::from_secs(120));
    let mut op_ids = Vec::new();
    for nonce in 0..3 {
        op_ids.push(
            submit(
                &rt,
                &c,
                &transfer_op(&alice, nonce, 2, bob.account, 10 + u128::from(nonce)),
            )
            .unwrap(),
        );
        op_ids.push(submit(&rt, &c, &transfer_op(&bob, nonce, 3, alice.account, 1)).unwrap());
    }
    for id in &op_ids {
        assert_eq!(
            wait_included(&rt, &c, *id, Duration::from_secs(120)).status,
            OpState::FastConfirmed
        );
    }
    let tip = latest(&rt, &c);
    wait_height(&rt, &c, tip.header.height + 2, Duration::from_secs(120));
    node.stop();

    // 2. Restart read-only after a normal stop (no block production): tip, blocks, accounts and op statuses are
    // identical
    let reader = NodeProcess::start(&data, &genesis, None, 1, &btc);
    let rc = reader.client();
    let before_restart = latest(&rt, &rc);
    let snapshot = |c: &jsonrpsee::http_client::HttpClient| {
        let tip = latest(&rt, c);
        let blocks: Vec<_> = (0..=tip.header.height)
            .map(|h| block_at(&rt, c, h))
            .collect();
        let accounts: Vec<_> = [&alice, &bob, &producer]
            .iter()
            .map(|u| account(&rt, c, u))
            .collect();
        let statuses: Vec<_> = op_ids
            .iter()
            .map(|id| rt.block_on(c.get_op_status(hex::encode(id))).unwrap())
            .collect();
        (tip, blocks, accounts, statuses)
    };
    let first = snapshot(&rc);
    assert_eq!(first.0, before_restart);
    // Available balances: the sponsored fees and transfers each one paid
    assert_eq!(ksa(&first.2[0]), 50_000 - (10 + 11 + 12) - 3 * 2 + 3);
    assert_eq!(ksa(&first.2[1]), 50_000 + (10 + 11 + 12) - 3 * 3 - 3);
    assert_eq!(first.2[0].nonce, 3);
    assert_eq!(first.2[1].nonce, 3);
    // The producer's income: sponsored fees plus each block's block reward (D-074); not yet unlocked in the locked
    // records, already unlocked in the available balance
    let locked: u128 = first.2[2].locked_fees.iter().map(|l| l.amount).sum();
    // Each block's number of reward slots is its weight (empty blocks 12, 24, ... by streak, D-084, D-090)
    let slots = chain_slots(first.0.header.height, |h| {
        first.1[usize::try_from(h).unwrap()].header.clone()
    });
    assert_eq!(
        locked + ksa(&first.2[2]),
        3 * 2 + 3 * 3 + slots * DEVNET.block_reward
    );
    reader.stop();

    let again = NodeProcess::start(&data, &genesis, None, 1, &btc);
    let second = snapshot(&again.client());
    assert_eq!(
        first, second,
        "the state read after the two restarts should be identical"
    );
    again.stop();

    // 3. SIGKILL in the middle of block production: after the restart the tip's state root matches the state store
    // (checked on open), and block production continues on the original chain
    let node = NodeProcess::start(&data, &genesis, Some(&producer), 1, &btc);
    let c = node.client();
    let resumed_from = latest(&rt, &c);
    assert!(resumed_from.header.height >= second.0.header.height);
    submit(&rt, &c, &transfer_op(&alice, 3, 2, bob.account, 5)).unwrap();
    wait_height(
        &rt,
        &c,
        resumed_from.header.height + 3,
        Duration::from_secs(120),
    );
    node.kill();

    let node = NodeProcess::start(&data, &genesis, Some(&producer), 1, &btc);
    let c = node.client();
    let after_crash = latest(&rt, &c);
    assert!(after_crash.header.height >= resumed_from.header.height + 3);
    let next = wait_height(
        &rt,
        &c,
        after_crash.header.height + 2,
        Duration::from_secs(120),
    );
    assert_eq!(
        block_at(&rt, &c, after_crash.header.height + 1)
            .header
            .parents,
        vec![after_crash.hash],
        "after the restart, blocks continue from the original tip"
    );
    assert!(next.header.height > after_crash.header.height);
    node.stop();

    // 4. Validate all blocks from scratch with another node instance: headers, difficulty, PoW, entries roots,
    // settlement payloads and the re-executed state roots all match
    let genesis_file = GenesisFile::load(&genesis).unwrap();
    let source = Chain::open(&data, DEVNET, &genesis_file, btc.client()).unwrap();
    let replay_dir = tempfile::tempdir().unwrap();
    let replica = Chain::open(replay_dir.path(), DEVNET, &genesis_file, btc.client()).unwrap();
    while replica.sync_btc().unwrap() {}
    let tip = source.tip().unwrap();
    for height in 1..=tip.header.height {
        let hash = source.store().hash_at(height).unwrap().unwrap();
        let block = source.store().block(&hash).unwrap().unwrap();
        replica.import_block(&block, None).unwrap();
    }
    assert_eq!(replica.tip().unwrap(), tip);
    drop(source);

    // 5. Open the same data directory with a different genesis config: startup is refused
    let other = write_genesis(replay_dir.path(), btc.anchor(), &[(&alice, 1)], None);
    let err = NodeProcess::try_start(&data, &other, None, 1, &btc)
        .err()
        .expect("startup should be refused");
    assert!(err.contains("genesis config"), "{err}");
}
