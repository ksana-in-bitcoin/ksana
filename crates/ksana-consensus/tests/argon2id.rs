//! The network's mining algorithm Argon2id (D-094): the shared vectors `tests/vectors/pow.json` (the TS SDK checks the
//! same file), block production and checks at the devnet difficulty, blocks produced with another algorithm are
//! rejected, every field enters the PoW value. The algorithm-independent rules are in `pow.rs` (with the test
//! algorithm `Sha256d`).

use std::sync::Arc;

use ksana_consensus::{
    check_pow, effective_target, empty_entries_root, genesis_header, pow_algorithm, Argon2id,
    Consensus, ConsensusError, PowAlgorithm, PowConsensus, Sha256d,
};
use ksana_primitives::{
    Argon2Params, BlockHeader, BtcAnchor, Params, PowAlgorithmId, BLOCK_VERSION, DEVNET,
};
use serde_json::Value;

const GENESIS_TIME: u64 = 1_700_000_000;

fn genesis(params: &Params) -> BlockHeader {
    genesis_header(
        params,
        GENESIS_TIME,
        BtcAnchor {
            height: 100,
            hash: [0xab; 32],
        },
        [0x5e; 32],
    )
    .unwrap()
}

fn child(
    consensus: &PowConsensus,
    parent: &BlockHeader,
    entries_root: [u8; 32],
    daa_score: u64,
    weight: u64,
) -> BlockHeader {
    BlockHeader {
        version: BLOCK_VERSION,
        height: parent.height + 1,
        parents: vec![parent.hash()],
        btc_anchor: parent.btc_anchor.clone(),
        entries_root,
        state_root: [0x22; 32],
        producer: [0x33; 32],
        timestamp: parent.timestamp + 5,
        bits: consensus.next_bits(parent, daa_score, weight).unwrap(),
        nonce: 0,
    }
}

fn load_vectors() -> Value {
    let path = format!(
        "{}/../../tests/vectors/pow.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn u32_field(value: &Value, key: &str) -> u32 {
    u32::try_from(value[key].as_u64().unwrap()).unwrap()
}

/// Shared vectors: the block header's encoding and hash, the salt, the Argon2id output and PoW value, the effective
/// target and whether it is met all agree with the file case by case; the network params give the same values.
#[test]
fn shared_vectors_match() {
    let vectors = load_vectors();
    assert_eq!(vectors["algorithm"], "argon2id");
    assert_eq!(vectors["version"], 0x13);
    assert_eq!(
        hex::decode(vectors["salt_prefix"].as_str().unwrap()).unwrap(),
        Argon2Params::SALT_PREFIX
    );
    let PowAlgorithmId::Argon2id(devnet_argon2) = DEVNET.pow_algorithm;
    assert_eq!(vectors["devnet"]["chain_id"], DEVNET.chain_id);
    assert_eq!(
        u32_field(&vectors["devnet"]["argon2"], "memory_kib"),
        devnet_argon2.memory_kib
    );
    assert_eq!(
        u32_field(&vectors["devnet"]["argon2"], "iterations"),
        devnet_argon2.iterations
    );
    assert_eq!(
        u32_field(&vectors["devnet"]["argon2"], "lanes"),
        devnet_argon2.lanes
    );

    let cases = vectors["cases"].as_array().unwrap();
    assert!(cases.len() >= 10, "{}", cases.len());
    let mut met = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let header: BlockHeader = serde_json::from_value(case["header"].clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&header).unwrap(),
            case["header"],
            "{name}"
        );
        assert_eq!(
            hex::encode(header.to_bytes()),
            case["header_borsh"],
            "{name}"
        );
        assert_eq!(hex::encode(header.hash()), case["block_hash"], "{name}");

        let chain_id = u32_field(case, "chain_id");
        let argon2 = Argon2Params {
            memory_kib: u32_field(&case["argon2"], "memory_kib"),
            iterations: u32_field(&case["argon2"], "iterations"),
            lanes: u32_field(&case["argon2"], "lanes"),
        };
        let algorithm = Argon2id::new(chain_id, argon2).unwrap();
        assert_eq!(hex::encode(algorithm.salt()), case["salt"], "{name}");
        let pow_hash = algorithm.pow_hash(&header).unwrap();
        assert_eq!(hex::encode(pow_hash), case["pow_hash"], "{name}");
        let pow_value = algorithm.pow_value(&header).unwrap();
        assert_eq!(pow_value.to_hex(), case["pow_value"], "{name}");

        let weight = case["weight"].as_u64().unwrap();
        let limit = u32_field(case, "pow_limit_bits");
        let target = effective_target(header.bits, limit, weight).unwrap();
        assert_eq!(target.to_hex(), case["effective_target"], "{name}");
        let meets = case["meets"].as_bool().unwrap();
        assert_eq!(pow_value <= target, meets, "{name}");
        assert_eq!(
            check_pow(&algorithm, &header, limit, weight),
            if meets {
                Ok(())
            } else {
                Err(ConsensusError::InsufficientPow)
            },
            "{name}"
        );
        met += usize::from(meets);

        // the network params (chain ID and algorithm parameters) give the same algorithm instance
        let params = Params {
            chain_id,
            pow_algorithm: PowAlgorithmId::Argon2id(argon2),
            ..DEVNET
        };
        assert_eq!(
            pow_algorithm(&params).unwrap().pow_value(&header).unwrap(),
            pow_value,
            "{name}"
        );
    }
    // the solved cases qualify; the cases with a target below the limit or another salt / parameters mostly do not
    assert!((2..cases.len()).contains(&met), "{met}");
}

/// Produces and checks blocks with the network algorithm at the devnet difficulty (the PoW limit): a non-empty block
/// and empty blocks with weight 12 and 24 are each solved with their own weight, and `check_header` passes; the
/// solution is the first nonce in the range that meets the effective target; the test algorithm Sha256d gives a
/// different value for the same block header.
#[test]
fn mines_and_validates_at_devnet_difficulty() {
    let g = genesis(&DEVNET);
    let consensus = PowConsensus::new(&DEVNET, &g).unwrap();
    assert_eq!(consensus.algorithm().name(), "argon2id");
    let limit = DEVNET.pow_limit_bits;

    let mut b1 = child(&consensus, &g, [0x11; 32], 1, 1);
    assert!(consensus.solve(&mut b1, 1, 0, 1 << 12).unwrap());
    assert_eq!(consensus.check_header(&g, 1, 1, &b1), Ok(()));
    let mut e1 = child(&consensus, &b1, empty_entries_root(), 1 + 12, 12);
    assert_eq!(consensus.weight(&e1, 0), 12);
    assert!(consensus.solve(&mut e1, 12, 0, 1 << 12).unwrap());
    assert_eq!(consensus.check_header(&b1, 13, 12, &e1), Ok(()));
    let mut e2 = child(&consensus, &e1, empty_entries_root(), 13 + 24, 24);
    assert_eq!(consensus.weight(&e2, 1), 24);
    assert!(consensus.solve(&mut e2, 24, 0, 1 << 12).unwrap());
    assert_eq!(consensus.check_header(&e1, 37, 24, &e2), Ok(()));

    for (header, weight) in [(&b1, 1u64), (&e1, 12), (&e2, 24)] {
        let target = effective_target(header.bits, limit, weight).unwrap();
        let algorithm = consensus.algorithm();
        assert!(algorithm.pow_value(header).unwrap() <= target);
        for nonce in 0..header.nonce {
            let earlier = BlockHeader {
                nonce,
                ..header.clone()
            };
            assert!(
                algorithm.pow_value(&earlier).unwrap() > target,
                "nonce {nonce} qualifies earlier"
            );
        }
        assert_ne!(
            Sha256d.pow_value(header).unwrap(),
            algorithm.pow_value(header).unwrap()
        );
    }
    // an empty block checked against the weight-1 target of course qualifies; a non-empty block checked with weight 12
    // most likely does not — only the certain relation is asserted here: the effective target decreases with the weight
    assert!(
        effective_target(e1.bits, limit, 12).unwrap()
            < effective_target(e1.bits, limit, 1).unwrap()
    );
}

/// A chain produced with the test algorithm Sha256d at a difficulty of about 1/512: under the network algorithm
/// Argon2id almost none of it qualifies (each block happens to qualify with probability about 1/512).
#[test]
fn blocks_mined_with_another_algorithm_are_rejected() {
    let mut g = genesis(&DEVNET);
    g.bits = 0x1f7f_ffff;
    let sha = PowConsensus::with_algorithm(&DEVNET, &g, Arc::new(Sha256d)).unwrap();
    let argon2id = PowConsensus::new(&DEVNET, &g).unwrap();
    assert_ne!(sha, argon2id);
    let mut parent = g.clone();
    let mut rejected = 0;
    for tag in 1..=8u8 {
        let mut header = child(&sha, &parent, [tag; 32], parent.height + 1, 1);
        assert!(sha.solve(&mut header, 1, 0, 1 << 24).unwrap());
        assert_eq!(sha.check_header(&parent, header.height, 1, &header), Ok(()));
        match argon2id.check_header(&parent, header.height, 1, &header) {
            Ok(()) => {}
            Err(ConsensusError::InsufficientPow) => rejected += 1,
            Err(e) => panic!("block {tag}: {e}"),
        }
        parent = header;
    }
    assert!(rejected >= 6, "{rejected}");
}

/// Every field of the block header enters the Argon2id message: tampering with any field changes the PoW value;
/// computing the same block header twice gives the same result.
#[test]
fn every_field_enters_the_pow_value() {
    let g = genesis(&DEVNET);
    let consensus = PowConsensus::new(&DEVNET, &g).unwrap();
    let header = child(&consensus, &g, [0x11; 32], 1, 1);
    let algorithm = consensus.algorithm();
    let base = algorithm.pow_value(&header).unwrap();
    assert_eq!(algorithm.pow_value(&header).unwrap(), base);
    let tampers: [&dyn Fn(&mut BlockHeader); 13] = [
        &|h| h.version += 1,
        &|h| h.height += 1,
        &|h| h.parents[0][0] ^= 1,
        &|h| h.parents.push([0xff; 32]),
        &|h| h.btc_anchor.height += 1,
        &|h| h.btc_anchor.hash[31] ^= 1,
        &|h| h.entries_root[0] ^= 1,
        &|h| h.state_root[15] ^= 1,
        &|h| h.producer[31] ^= 1,
        &|h| h.timestamp += 1,
        &|h| h.bits ^= 1,
        &|h| h.nonce += 1,
        &|h| h.nonce ^= 1 << 63,
    ];
    for (i, tamper) in tampers.iter().enumerate() {
        let mut tampered = header.clone();
        tamper(&mut tampered);
        assert_ne!(algorithm.pow_value(&tampered).unwrap(), base, "tamper #{i}");
    }
}
