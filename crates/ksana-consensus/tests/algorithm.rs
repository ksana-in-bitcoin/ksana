//! The mining algorithm is decoupled from consensus (D-062): the network params select Argon2id (D-094); with another
//! algorithm, solving, block header checks and work proceed as usual; the range semantics of the default solve.
//! The tests of Argon2id itself (shared vectors, block production at the network difficulty, tampering) are in
//! `argon2id.rs`; the algorithm-independent rules (`bits`, target, weight, solve range) are checked in `pow.rs` with
//! the test algorithm `Sha256d`.

use std::sync::Arc;

use ksana_consensus::{
    check_pow, genesis_header, pow_algorithm, target_from_bits, Argon2id, Consensus,
    ConsensusError, PowAlgorithm, PowConsensus, Sha256d, U256,
};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Argon2Params, BlockHeader, BtcAnchor, Params, PowAlgorithmId, BLOCK_VERSION, DEVNET, SIGNET,
};

/// Test algorithm: PoW value = a single SHA-256 of the block header encoding interpreted as big-endian. Only
/// `pow_value` is implemented; solving uses the trait's default implementation.
#[derive(Debug)]
struct Sha256Be;

impl PowAlgorithm for Sha256Be {
    fn name(&self) -> &'static str {
        "sha256-be"
    }

    fn pow_value(&self, header: &BlockHeader) -> Result<U256, ConsensusError> {
        let digest = sha256(&header.to_bytes());
        let mut le = digest;
        le.reverse();
        Ok(U256::from_le_bytes(le))
    }
}

fn params() -> Params {
    DEVNET
}

/// Genesis difficulty about 1/512 (`bits = 0x1f7fffff`, target about 2^247; the PoW limit is still devnet's value):
/// the probability that a block happens to satisfy both algorithms at once is small.
fn genesis(params: &Params) -> BlockHeader {
    let mut g = genesis_header(
        params,
        1_700_000_000,
        BtcAnchor {
            height: 100,
            hash: [0xab; 32],
        },
        [0x5e; 32],
    )
    .unwrap();
    g.bits = 0x1f7f_ffff;
    g
}

fn child(consensus: &PowConsensus, parent: &BlockHeader, tag: u8) -> BlockHeader {
    BlockHeader {
        version: BLOCK_VERSION,
        height: parent.height + 1,
        parents: vec![parent.hash()],
        btc_anchor: parent.btc_anchor.clone(),
        entries_root: [tag; 32],
        state_root: [0x22; 32],
        producer: [0x33; 32],
        timestamp: parent.timestamp + 5,
        bits: chain_next_bits(consensus, parent),
        nonce: 0,
    }
}

/// The `bits` of a non-empty block (weight 1) on a pure chain: the DAA score equals the height (the weight does not
/// affect `bits`, D-084).
fn chain_next_bits(consensus: &PowConsensus, parent: &BlockHeader) -> u32 {
    consensus.next_bits(parent, parent.height + 1, 1).unwrap()
}

/// The block header check of a non-empty block on a pure chain: the DAA score equals the height.
fn check_chain_header(
    consensus: &PowConsensus,
    parent: &BlockHeader,
    header: &BlockHeader,
) -> Result<(), ConsensusError> {
    consensus.check_header(parent, header.height, 1, header)
}

/// The network params select Argon2id (D-094): devnet and signet have the same parameters and differ in the salt by
/// chain ID; `PowConsensus::new` uses it; invalid parameters are a configuration error.
#[test]
fn network_parameter_selects_argon2id() {
    let argon2 = Argon2Params {
        memory_kib: 2048,
        iterations: 1,
        lanes: 1,
    };
    assert_eq!(params().pow_algorithm, PowAlgorithmId::Argon2id(argon2));
    assert_eq!(SIGNET.pow_algorithm, DEVNET.pow_algorithm);
    assert_eq!(pow_algorithm(&DEVNET).unwrap().name(), "argon2id");
    let g = genesis(&params());
    let consensus = PowConsensus::new(&params(), &g).unwrap();
    assert_eq!(consensus.algorithm().name(), "argon2id");
    assert_eq!(consensus.solve_chunk(), 16);

    // PoW value = the Argon2id output interpreted as little-endian, salt "ksana-pow" ‖ chain ID; signet's value differs
    let devnet = Argon2id::new(DEVNET.chain_id, argon2).unwrap();
    let signet = Argon2id::new(SIGNET.chain_id, argon2).unwrap();
    assert_eq!(devnet.salt(), b"ksana-pow\xe9\x03\x00\x00");
    assert_eq!(signet.salt(), b"ksana-pow\xea\x03\x00\x00");
    let hash = devnet.pow_hash(&g).unwrap();
    assert_eq!(
        consensus.algorithm().pow_value(&g).unwrap(),
        U256::from_le_bytes(hash)
    );
    assert_eq!(
        pow_algorithm(&SIGNET).unwrap().pow_value(&g).unwrap(),
        U256::from_le_bytes(signet.pow_hash(&g).unwrap())
    );
    assert_ne!(signet.pow_hash(&g).unwrap(), hash);
    // the test-only Sha256d is the block hash interpreted as little-endian (D-025)
    assert_eq!(
        Sha256d.pow_value(&g).unwrap(),
        U256::from_le_bytes(g.hash())
    );

    let bad = Params {
        pow_algorithm: PowAlgorithmId::Argon2id(Argon2Params {
            memory_kib: 4,
            ..argon2
        }),
        ..DEVNET
    };
    assert!(matches!(
        PowConsensus::new(&bad, &g),
        Err(ConsensusError::InvalidParams(_))
    ));
}

/// Switching to the test algorithm: the solved chain passes the block header check under that algorithm and fails
/// under Sha256d; work and `bits` do not change with the algorithm.
#[test]
fn another_algorithm_mines_and_validates_a_chain() {
    let params = params();
    let g = genesis(&params);
    let custom = PowConsensus::with_algorithm(&params, &g, Arc::new(Sha256Be)).unwrap();
    let sha = PowConsensus::with_algorithm(&params, &g, Arc::new(Sha256d)).unwrap();
    assert_eq!(custom.algorithm().name(), "sha256-be");
    assert_ne!(custom, sha);

    let mut parent = g.clone();
    let mut rejected_by_sha256d = 0;
    for tag in 1..=20u8 {
        let mut header = child(&custom, &parent, tag);
        assert_eq!(header.bits, chain_next_bits(&sha, &parent));
        assert!(
            custom.solve(&mut header, 1, 0, 1 << 24).unwrap(),
            "block {tag}"
        );
        assert_eq!(check_chain_header(&custom, &parent, &header), Ok(()));
        let target = target_from_bits(header.bits, params.pow_limit_bits).unwrap();
        assert!(Sha256Be.pow_value(&header).unwrap() <= target);
        match check_chain_header(&sha, &parent, &header) {
            Ok(()) => {}
            Err(ConsensusError::InsufficientPow) => rejected_by_sha256d += 1,
            Err(e) => panic!("block {tag}: {e}"),
        }
        assert_eq!(custom.work(&header, 1), sha.work(&header, 1));
        parent = header;
    }
    assert!(rejected_by_sha256d >= 18, "{rejected_by_sha256d}");

    // conversely: a block solved under Sha256d most likely does not qualify under the test algorithm
    let mut header = child(&sha, &g, 0x77);
    assert!(sha.solve(&mut header, 1, 0, 1 << 24).unwrap());
    assert_eq!(check_chain_header(&sha, &g, &header), Ok(()));
    assert_eq!(
        check_pow(&Sha256d, &header, params.pow_limit_bits, 1),
        Ok(())
    );
    assert_eq!(
        check_pow(&Sha256Be, &header, params.pow_limit_bits, 1),
        Err(ConsensusError::InsufficientPow)
    );
}

/// Default solve: what is found is the first qualifying nonce from `start` on; the block header is unchanged when
/// nothing is found; `iterations = 0` tries nothing.
#[test]
fn default_solve_returns_the_first_nonce_in_range() {
    let params = params();
    let g = genesis(&params);
    let consensus = PowConsensus::with_algorithm(&params, &g, Arc::new(Sha256Be)).unwrap();
    let template = child(&consensus, &g, 9);
    let target = target_from_bits(template.bits, params.pow_limit_bits).unwrap();

    let mut header = template.clone();
    assert!(Sha256Be.solve(&mut header, &target, 1000, 1 << 24).unwrap());
    let found = header.nonce;
    assert!(found >= 1000);
    for nonce in 1000..found {
        let h = BlockHeader {
            nonce,
            ..template.clone()
        };
        assert!(Sha256Be.pow_value(&h).unwrap() > target, "nonce {nonce}");
    }
    // the range ends right before the solution: nothing found, nonce unchanged
    let mut header = template.clone();
    assert!(!Sha256Be
        .solve(&mut header, &target, 1000, found - 1000)
        .unwrap());
    assert_eq!(header, template);
    assert!(!Sha256Be.solve(&mut header, &U256::MAX, 0, 0).unwrap());
    assert_eq!(header, template);
    // up to u64::MAX, no wrap-around
    assert!(Sha256Be
        .solve(&mut header, &U256::MAX, u64::MAX, 10)
        .unwrap());
    assert_eq!(header.nonce, u64::MAX);
    // the default number of attempts per call
    assert_eq!(Sha256Be.solve_chunk(), 1 << 14);
    assert_eq!(consensus.solve_chunk(), 1 << 14);
}
