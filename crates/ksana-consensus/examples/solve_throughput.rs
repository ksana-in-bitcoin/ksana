//! Measures the single-threaded verification and solving speed of the network's mining algorithm (Argon2id, D-094):
//!
//! ```sh
//! cargo run -p ksana-consensus --release --example solve_throughput
//! ```
//!
//! Verification: `pow_value` one by one over a batch of block headers with different nonces (allocating memory each
//! time, the same as a node checking block headers); solving: the target is 0 (impossible to meet), so each call tries
//! exactly the given number of nonces (reusing memory). As a reference, the speed of the test-only `Sha256d` is also
//! measured.

// This example measures elapsed time and needs to read the system clock (forbidden in consensus code, see clippy.toml).
#![allow(clippy::disallowed_methods)]

use std::time::Instant;

use ksana_consensus::{genesis_header, pow_algorithm, PowAlgorithm, Sha256d, U256};
use ksana_primitives::{BlockHeader, BtcAnchor, PowAlgorithmId, DEVNET};

fn main() {
    let anchor = BtcAnchor {
        height: 1,
        hash: [1; 32],
    };
    let genesis =
        genesis_header(&DEVNET, 1_700_000_000, anchor, [2; 32]).expect("genesis block header");
    let mut header = BlockHeader {
        height: 1,
        parents: vec![genesis.hash()],
        ..genesis
    };
    let PowAlgorithmId::Argon2id(params) = DEVNET.pow_algorithm;
    println!(
        "Argon2id: memory {} KiB, passes {}, parallelism {}",
        params.memory_kib, params.iterations, params.lanes
    );
    let argon2id = pow_algorithm(&DEVNET).expect("network params");

    let verifications = 500u64;
    let start = Instant::now();
    for nonce in 0..verifications {
        header.nonce = nonce;
        let value = argon2id.pow_value(&header).expect("compute PoW value");
        assert!(value > U256::ZERO);
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "verification (memory allocated per block header): {verifications} block headers in {secs:.3} s, {:.3} ms each, {:.0} per second",
        secs * 1e3 / verifications as f64,
        verifications as f64 / secs
    );

    let attempts = 2_000u64;
    let start = Instant::now();
    let found = argon2id
        .solve(&mut header, &U256::ZERO, 0, attempts)
        .expect("solve");
    let secs = start.elapsed().as_secs_f64();
    assert!(!found);
    println!(
        "solving (memory reused): {attempts} attempts in {secs:.3} s, {:.3} ms each, {:.0} per second",
        secs * 1e3 / attempts as f64,
        attempts as f64 / secs
    );

    let sha_attempts = 2_000_000u64;
    let start = Instant::now();
    let found = Sha256d
        .solve(&mut header, &U256::ZERO, 0, sha_attempts)
        .expect("solve");
    let secs = start.elapsed().as_secs_f64();
    assert!(!found);
    println!(
        "reference (test-only Sha256d): {sha_attempts} attempts in {secs:.3} s, {:.2} M per second",
        sha_attempts as f64 / secs / 1e6
    );
}
