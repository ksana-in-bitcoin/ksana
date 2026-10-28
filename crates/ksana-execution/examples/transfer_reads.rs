//! Measures what the status reads of D-121 add to a transfer (the review of D-121, S1):
//!
//! ```sh
//! cargo run -p ksana-execution --release --example transfer_reads
//! ```
//!
//! Over a state of 10 000 accounts holding two assets, committed to RocksDB as the node and the indexer keep it, a
//! block of 5 000 transfers between distinct accounts of the asset without capabilities, which reads the asset's record
//! and the two balances, is executed and committed against the same block of the asset with `freeze` and `allowlist`,
//! which also reads the two parties' status keys. The gas table charges both the flat 10 000 of `AssetTransfer`
//! (D-013, D-121).

// This example measures elapsed time and needs to read the system clock (forbidden in consensus code, see clippy.toml).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ksana_execution::{builtin, MemState, Overlay};
use ksana_primitives::asset::{CAP_ALLOWLIST, CAP_FREEZE, STATUS_ALLOWED};
use ksana_primitives::hash::sha256;
use ksana_state::StateDb;
use ksana_vm::AssetMetaInput;

const ACCOUNTS: u32 = 10_000;
const ROUNDS: usize = 5;
const ISSUER: [u8; 32] = [1; 32];

fn account(i: u32) -> [u8; 32] {
    sha256(&i.to_le_bytes())
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn per_transfer(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6 / f64::from(ACCOUNTS / 2)
}

fn main() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let mut db = StateDb::open(dir.path()).expect("state store");
    let meta = AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    };
    let plain = builtin::asset_id(&ISSUER, &[1; 32]);
    let guarded = builtin::asset_id(&ISSUER, &[2; 32]);
    let supply = u128::from(ACCOUNTS) * 1_000;
    let mut ov = Overlay::new(Arc::new(MemState::default()));
    builtin::create(&mut ov, &plain, &ISSUER, &meta, 0, &ISSUER, supply).expect("plain asset");
    let caps = CAP_FREEZE | CAP_ALLOWLIST;
    builtin::create(&mut ov, &guarded, &ISSUER, &meta, caps, &ISSUER, supply)
        .expect("guarded asset");
    for i in 0..ACCOUNTS {
        let a = account(i);
        builtin::set_status(&mut ov, &guarded, &ISSUER, &a, STATUS_ALLOWED, true).expect("allow");
        for asset in [&plain, &guarded] {
            builtin::transfer(&mut ov, asset, &ISSUER, &a, 1_000).expect("funding");
        }
    }
    ov.commit_op();
    let mut version = 1;
    db.commit(version, &ov.into_block_writes()).expect("commit");

    // [execution, commit] per asset, one sample per round
    let mut samples = [[vec![], vec![]], [vec![], vec![]]];
    for round in 0..ROUNDS {
        // each asset goes first in every other round
        for which in [round % 2, 1 - round % 2] {
            let asset = [&plain, &guarded][which];
            let times = &mut samples[which];
            let view = db.view().expect("view");
            let mut ov = Overlay::new(Arc::new(view));
            let start = Instant::now();
            for k in 0..ACCOUNTS / 2 {
                builtin::transfer(&mut ov, asset, &account(2 * k), &account(2 * k + 1), 1)
                    .expect("transfer");
                ov.commit_op();
            }
            times[0].push(start.elapsed());
            let writes = ov.into_block_writes();
            version += 1;
            let start = Instant::now();
            db.commit(version, &writes).expect("commit");
            times[1].push(start.elapsed());
        }
    }
    let [plain_times, guarded_times] = samples.map(|t| t.map(median));
    println!(
        "{ACCOUNTS} accounts holding both assets in RocksDB; blocks of {} transfers between distinct accounts; \
         median of {ROUNDS} rounds",
        ACCOUNTS / 2
    );
    for (name, [execution, commit]) in [
        ("without capabilities", plain_times),
        ("with freeze and allowlist", guarded_times),
    ] {
        println!(
            "asset {name}: execution {:.2} µs, commit {:.2} µs per transfer",
            per_transfer(execution),
            per_transfer(commit)
        );
    }
    let ratio = |a: Duration, b: Duration| (b.as_secs_f64() / a.as_secs_f64() - 1.0) * 100.0;
    println!(
        "the status reads add {:.0}% to the execution, {:.0}% to execution and commit together",
        ratio(plain_times[0], guarded_times[0]),
        ratio(
            plain_times[0] + plain_times[1],
            guarded_times[0] + guarded_times[1]
        )
    );
}
