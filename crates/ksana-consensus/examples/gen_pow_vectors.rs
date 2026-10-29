//! Generates the PoW test vectors `tests/vectors/pow.json` shared by Rust and the TypeScript SDK (D-094):
//!
//! ```sh
//! cargo run -p ksana-consensus --example gen_pow_vectors
//! ```
//!
//! Once committed, the vectors are the reference; regenerate them only when the definition of the PoW (algorithm, salt,
//! parameters, interpretation of the output) is changed on purpose, and record it in `docs/DECISIONS.md`.
//! Each case gives the block header (the JSON of D-007 and the Borsh encoding), the chain ID and the Argon2id
//! parameters, the salt, the block hash, the 32-byte Argon2id output `pow_hash`, the `pow_value` interpreted as
//! little-endian, the weight and the effective target, and whether the PoW is met; Rust checks them one by one in
//! `tests/argon2id.rs`, the TS SDK in `sdk/ts/test/pow.test.ts`.

use ksana_consensus::{
    effective_target, empty_entries_root, genesis_header, Argon2id, Consensus, PowConsensus,
};
use ksana_primitives::{
    Argon2Params, BlockHeader, BtcAnchor, Params, PowAlgorithmId, BLOCK_VERSION, DEVNET, SIGNET,
};
use serde_json::{json, Value};

/// One case: `chain_id` and `argon2` determine the algorithm instance, `weight` determines the effective target.
struct Case {
    name: &'static str,
    note: &'static str,
    params: Params,
    header: BlockHeader,
    weight: u64,
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
        bits: consensus
            .next_bits(parent, daa_score, weight)
            .expect("bits"),
        nonce: 0,
    }
}

fn with_argon2(params: &Params, argon2: Argon2Params) -> Params {
    Params {
        pow_algorithm: PowAlgorithmId::Argon2id(argon2),
        ..params.clone()
    }
}

fn case_json(case: &Case) -> Value {
    let PowAlgorithmId::Argon2id(argon2) = case.params.pow_algorithm;
    let algorithm = Argon2id::new(case.params.chain_id, argon2).expect("algorithm params");
    let pow_hash = algorithm.pow_hash(&case.header).expect("compute PoW value");
    let pow_value = ksana_consensus::pow_value(&pow_hash);
    let target = effective_target(case.header.bits, case.params.pow_limit_bits, case.weight)
        .expect("effective target");
    json!({
        "name": case.name,
        "note": case.note,
        "chain_id": case.params.chain_id,
        "argon2": {
            "memory_kib": argon2.memory_kib,
            "iterations": argon2.iterations,
            "lanes": argon2.lanes,
        },
        "salt": hex::encode(algorithm.salt()),
        "pow_limit_bits": case.params.pow_limit_bits,
        "header": serde_json::to_value(&case.header).expect("JSON of the block header"),
        "header_borsh": hex::encode(case.header.to_bytes()),
        "block_hash": hex::encode(case.header.hash()),
        "pow_hash": hex::encode(pow_hash),
        "pow_value": pow_value.to_hex(),
        "weight": case.weight,
        "effective_target": target.to_hex(),
        "meets": pow_value <= target,
    })
}

fn main() {
    let genesis = genesis_header(
        &DEVNET,
        1_700_000_000,
        BtcAnchor {
            height: 100,
            hash: [0xab; 32],
        },
        [0x5e; 32],
    )
    .expect("genesis block header");
    let consensus = PowConsensus::new(&DEVNET, &genesis).expect("consensus");

    // non-empty block (weight 1, DAA score 1): solved at devnet's minimum difficulty
    let mut nonempty = child(&consensus, &genesis, [0x11; 32], 1, 1);
    assert!(consensus
        .solve(&mut nonempty, 1, 0, 1 << 20)
        .expect("solve"));
    let next_nonce = BlockHeader {
        nonce: nonempty.nonce + 1,
        ..nonempty.clone()
    };
    // empty block (streak 1, weight 12, DAA score 12)
    let mut empty = child(&consensus, &genesis, empty_entries_root(), 12, 12);
    assert!(consensus.solve(&mut empty, 12, 0, 1 << 20).expect("solve"));
    // three parents (selected parent first, the rest ascending), nonce at the maximum, unsolved
    let three_parents = BlockHeader {
        parents: vec![genesis.hash(), [0x01; 32], [0xfe; 32]],
        nonce: u64::MAX,
        ..nonempty.clone()
    };
    // a target below the limit (2^-16): almost impossible to meet
    let lower_target = BlockHeader {
        bits: 0x1f00_ffff,
        ..nonempty.clone()
    };

    let cases = [
        Case {
            name: "devnet-genesis",
            note: "devnet genesis block header (no parents, empty block, weight 12, nonce 0, D-027)",
            params: DEVNET,
            header: genesis.clone(),
            weight: 12,
        },
        Case {
            name: "devnet-nonempty-solved",
            note: "non-empty block after the genesis block, solved at devnet's minimum difficulty (0x207fffff) with weight 1",
            params: DEVNET,
            header: nonempty.clone(),
            weight: 1,
        },
        Case {
            name: "devnet-nonempty-next-nonce",
            note: "the same block header, nonce plus 1",
            params: DEVNET,
            header: next_nonce,
            weight: 1,
        },
        Case {
            name: "devnet-empty-weight-12-solved",
            note: "empty block after the genesis block (streak 1, weight 12), solved against the effective target (target / 12)",
            params: DEVNET,
            header: empty.clone(),
            weight: 12,
        },
        Case {
            name: "devnet-empty-checked-with-weight-24",
            note: "the same empty block checked with weight 24 (streak 2): the effective target is halved",
            params: DEVNET,
            header: empty,
            weight: 24,
        },
        Case {
            name: "devnet-three-parents-max-nonce",
            note: "block header with three parents, nonce = 2^64 − 1, unsolved",
            params: DEVNET,
            header: three_parents,
            weight: 1,
        },
        Case {
            name: "devnet-lower-target",
            note: "bits = 0x1f00ffff (target about 2^240, below the limit), the same non-empty block header",
            params: DEVNET,
            header: lower_target,
            weight: 1,
        },
        Case {
            name: "signet-salt",
            note: "the same non-empty block header with signet's chain ID 1002: different salt, different value \
                   (judged against devnet's PoW limit, since its bits are above signet's, D-136)",
            // The header's bits are devnet's limit, above signet's own (D-136): the case is about the salt
            params: Params {
                pow_limit_bits: DEVNET.pow_limit_bits,
                ..SIGNET
            },
            header: nonempty.clone(),
            weight: 1,
        },
        Case {
            name: "argon2-64kib-2-passes-2-lanes",
            note: "the same non-empty block header, chain ID 1001, Argon2id parameters changed to 64 KiB, 2 passes, 2 lanes",
            params: with_argon2(
                &DEVNET,
                Argon2Params {
                    memory_kib: 64,
                    iterations: 2,
                    lanes: 2,
                },
            ),
            header: nonempty.clone(),
            weight: 1,
        },
        Case {
            name: "argon2-8kib-3-passes-1-lane",
            note: "the same non-empty block header, chain ID 1001, Argon2id parameters changed to the minimum memory 8 KiB, 3 passes, 1 lane",
            params: with_argon2(
                &DEVNET,
                Argon2Params {
                    memory_kib: 8,
                    iterations: 3,
                    lanes: 1,
                },
            ),
            header: nonempty,
            weight: 1,
        },
    ];

    let PowAlgorithmId::Argon2id(devnet_argon2) = DEVNET.pow_algorithm;
    let vectors = json!({
        "description": "Ksana PoW test vectors (D-094): pow_hash = Argon2id(version 0x13, message = header_borsh, salt = salt_prefix ‖ chain_id as 4 little-endian bytes, memory memory_kib, passes iterations, parallelism lanes, output 32 bytes); pow_value = pow_hash interpreted as little-endian; meets = pow_value ≤ effective_target = target(bits) / weight",
        "algorithm": "argon2id",
        "version": 0x13,
        "salt_prefix": hex::encode(Argon2Params::SALT_PREFIX),
        "devnet": {
            "chain_id": DEVNET.chain_id,
            "argon2": {
                "memory_kib": devnet_argon2.memory_kib,
                "iterations": devnet_argon2.iterations,
                "lanes": devnet_argon2.lanes,
            },
        },
        "cases": cases.iter().map(case_json).collect::<Vec<_>>(),
    });
    let path = format!(
        "{}/../../tests/vectors/pow.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut text = serde_json::to_string_pretty(&vectors).expect("JSON");
    text.push('\n');
    std::fs::write(&path, text).expect("write vectors");
    println!("wrote {path}: {} cases", cases.len());
}
