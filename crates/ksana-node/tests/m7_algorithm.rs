//! The mining algorithm is replaceable (D-062): when nodes switch to a test algorithm, block production, validation and
//! sync work as usual (two in-process nodes connected over a real libp2p network, BTC is the in-memory mock chain); a
//! node using the algorithm selected by the network params (Argon2id, D-094) rejects these blocks. The same scenario
//! for the indexer is in `crates/ksana-indexer/tests/indexer.rs`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::net::{wait, Net};
use common::{transfer_op, user};
use ksana_consensus::{check_pow, pow_algorithm, ConsensusError, PowAlgorithm, U256};
use ksana_node::chain::Chain;
use ksana_node::NodeError;
use ksana_primitives::hash::sha256;
use ksana_primitives::{BlockHeader, DEVNET};
use ksana_rpc::OpState;

/// Test algorithm: PoW value = a single SHA-256 of the encoded header interpreted as big-endian. Only `pow_value` is
/// implemented; solving uses the trait's default implementation.
#[derive(Debug)]
struct Sha256Be;

impl PowAlgorithm for Sha256Be {
    fn name(&self) -> &'static str {
        "sha256-be"
    }

    fn pow_value(&self, header: &BlockHeader) -> Result<U256, ConsensusError> {
        let mut digest = sha256(&header.to_bytes());
        digest.reverse();
        Ok(U256::from_le_bytes(digest))
    }
}

/// Genesis difficulty about 1/512 (`bits = 0x1f7fffff`): a block is very unlikely to satisfy both algorithms at once.
const BITS: u32 = 0x1f7f_ffff;

#[test]
fn another_pow_algorithm_mines_validates_and_syncs() {
    let algorithm: Arc<dyn PowAlgorithm> = Arc::new(Sha256Be);
    let (alice, bob) = (user("m7/algo/alice"), user("m7/algo/bob"));
    let mut net = Net::start_custom(
        2,
        &[(&alice, 1_000)],
        Duration::from_millis(100),
        Some(BITS),
        Some(algorithm.clone()),
    );
    let op = net.nodes[0].submit(&transfer_op(&alice, 0, 2, bob.account, 10));
    net.mine_on(0, 6);
    let tip = net.converge();
    assert_eq!(
        net.nodes[1].status(&op).unwrap().status,
        OpState::FastConfirmed
    );
    assert_eq!(net.nodes[1].ksa(&bob), 10);

    // Every block satisfies the test algorithm's PoW; almost none of them satisfy that of the network algorithm
    // Argon2id
    let network = pow_algorithm(&DEVNET).unwrap();
    let chain = net.nodes[1].node().chain();
    let blocks: Vec<_> = (1..=tip.header.height)
        .map(|h| {
            let hash = chain.store().hash_at(h).unwrap().unwrap();
            chain.store().block(&hash).unwrap().unwrap()
        })
        .collect();
    let mut first_network_failure = None;
    for block in &blocks {
        let weight = chain.weight_of(&block.header.hash()).unwrap().unwrap();
        check_pow(
            algorithm.as_ref(),
            &block.header,
            DEVNET.pow_limit_bits,
            weight,
        )
        .unwrap();
        if first_network_failure.is_none()
            && check_pow(
                network.as_ref(),
                &block.header,
                DEVNET.pow_limit_bits,
                weight,
            )
            .is_err()
        {
            first_network_failure = Some(block.header.height);
        }
    }
    let failing = first_network_failure.expect("at least one block fails Argon2id");

    // A node joining later (same algorithm) syncs from the genesis block
    let late = net.join();
    wait("new node syncs", Duration::from_secs(60), || {
        (net.nodes[late].tip() == tip).then_some(())
    });

    // A node using the algorithm selected by the network params (Argon2id): earlier blocks are accepted as usual, the
    // first block that fails Argon2id is judged invalid
    let dir = tempfile::tempdir().unwrap();
    let default = Chain::open(dir.path(), DEVNET, &net.genesis, net.mock.clone()).unwrap();
    while default.sync_btc().unwrap() {}
    for block in &blocks {
        let result = default.accept_block(block.clone(), None);
        if block.header.height < failing {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(NodeError::InvalidBlock(ref m)) if m.contains("PoW")),
                "{result:?}"
            );
            break;
        }
    }
}
