//! M11: network upgrade (D-076).
//!
//! - A single node crosses the activation point: the version of produced block headers switches, the RPC gives the
//!   protocol version and the next upgrade; VmOps of the new version are not accepted before activation and execute as
//!   usual after it.
//! - A node without the upgrade (its upgrade table lacks the entry) stops before the fork: blocks with a higher
//!   version are refused but not judged invalid and the peer is not penalized; after a restart with the upgraded params
//!   it syncs to the same chain.
//!
//! The activation point is by DAA score: an empty block has weight 12 (D-084), and on a single producer's chain the DAA
//! score accumulates the blocks' weights and no longer equals the height.

mod common;

use std::time::Duration;

use common::mock_node::{wait, MockNode};
use common::net::Net;
use common::user;
use ksana_crypto::sign_vmop;
use ksana_primitives::{Action, FeeMode, Params, SigScheme, Upgrade, VmOp, DEVNET, KSA_ASSET_ID};
use ksana_rpc::{OpState, UpgradeJson};

/// From DAA score `daa_score`: protocol version 3, accepting VmOp versions 2-3.
fn upgraded(daa_score: u64) -> Params {
    let upgrades: &'static [Upgrade] = Box::leak(Box::new([Upgrade {
        max_vmop_version: Some(3),
        ..Upgrade::new(3, daa_score)
    }]));
    Params { upgrades, ..DEVNET }
}

/// A path-B transfer with VmOp version `version`.
fn transfer(u: &common::User, nonce: u64, version: u8, to: [u8; 32], amount: u128) -> VmOp {
    let mut op = VmOp {
        version,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee: FeeMode::Token { amount: 5 },
        max_gas: 100_000,
        action: Action::AssetTransfer {
            asset: KSA_ASSET_ID,
            to,
            amount,
        },
        sig: SigScheme::Bip340([0; 64]),
    };
    op.sig = sign_vmop(&op, &u.kp, &[2; 32]).unwrap();
    op
}

#[test]
fn the_protocol_version_switches_at_the_activation_point() {
    // Crossed after about 50 empty blocks
    const ACTIVATION: u64 = 600;
    let (alice, bob) = (user("m11/alice"), user("m11/bob"));
    let n = MockNode::start_with_params(
        upgraded(ACTIVATION),
        &[(&alice, 1_000)],
        Duration::from_millis(100),
        |mock| mock,
        |funding| funding,
    );
    // Before activation: the RPC gives the next upgrade; version 3 VmOps are not accepted
    let info = n.chain_info();
    assert!(info.daa_score < ACTIVATION, "{info:?}");
    assert_eq!((info.protocol_version, info.max_known_version), (2, 3));
    assert_eq!(
        info.next_upgrade,
        Some(UpgradeJson {
            version: 3,
            daa_score: ACTIVATION
        })
    );
    let v3 = transfer(&alice, 0, 3, bob.account, 7);
    let err = n.submit_err(&v3);
    assert!(
        err.to_string().contains("unsupported VmOp version 3"),
        "{err}"
    );

    // Crossing the activation point: the header version switches at the block whose DAA score reaches the activation
    // point
    wait("activation point crossed", || {
        (n.chain_info().daa_score >= ACTIVATION + 60).then_some(())
    });
    let chain = n.node().chain();
    let mut switched_at = None;
    for height in 1..=n.tip_height() {
        let block = n.block(height);
        let daa_score = chain.ghostdag(&block.hash).unwrap().unwrap().daa_score;
        let expected = if daa_score < ACTIVATION { 2 } else { 3 };
        assert_eq!(
            block.header.version, expected,
            "height {height}, DAA score {daa_score}"
        );
        if expected == 3 && switched_at.is_none() {
            switched_at = Some(height);
        }
    }
    let switched_at = switched_at.expect("activation point crossed");
    assert!(
        switched_at > 1,
        "there should be old-version blocks before the activation point"
    );
    let info = n.chain_info();
    assert_eq!((info.protocol_version, info.next_upgrade), (3, None));

    // After activation, version 3 VmOps execute as usual
    let status = n.wait_included(&n.submit(&v3));
    assert_eq!(status.status, OpState::FastConfirmed);
    assert_eq!(n.ksa(&bob), 7);
}

#[test]
fn an_outdated_node_stops_before_the_fork_and_follows_after_upgrading() {
    // Crossed after about 17 empty blocks
    const ACTIVATION: u64 = 200;
    let alice = user("m11/net/alice");
    let mut net = Net::start_with_params(
        1,
        &[(&alice, 1_000)],
        Duration::from_millis(50),
        upgraded(ACTIVATION),
    );
    // The node without the upgrade: DEVNET's upgrade table has no version 3
    let old = net.join_with(DEVNET);
    net.wait_connected(&[0, old]);
    net.mine_on(0, 30);
    let tip = net.nodes[0].tip();
    assert_eq!(tip.header.version, 3);
    // Fork height: the first block whose DAA score reaches the activation point (version 3)
    let chain = net.nodes[0].node().chain();
    let fork = (1..=tip.header.height)
        .find(|height| {
            let hash = chain.store().hash_at(*height).unwrap().unwrap();
            chain.ghostdag(&hash).unwrap().unwrap().daa_score >= ACTIVATION
        })
        .expect("activation point crossed");
    assert!(fork > 1 && fork < tip.header.height, "fork height {fork}");

    // It stops at the last block before the fork and refuses later blocks without judging them invalid or penalizing
    // the peer: the two nodes remain connected
    common::net::wait(
        "old node syncs up to before the fork",
        Duration::from_secs(60),
        || (net.nodes[old].height() == fork - 1).then_some(()),
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(net.nodes[old].height(), fork - 1);
    assert!(net.nodes[old]
        .net()
        .connected_peers()
        .contains(&net.nodes[0].peer_id));
    let info = net.nodes[old].chain_info();
    assert_eq!((info.protocol_version, info.max_known_version), (2, 2));

    // After upgrading (restart with new params) it syncs to the same chain and follows as usual afterwards
    net.restart(old, upgraded(ACTIVATION));
    assert_eq!(net.converge().hash, tip.hash);
    net.mine_on(0, 3);
    net.converge();
    assert_eq!(net.nodes[old].chain_info().protocol_version, 3);
}
