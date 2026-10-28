//! The stable sweep of the differential test (D-133, `src/differential/`): the repository's vectors as they are, then
//! requests built from random values of each kind's structure and mutated now and then, all run through the Rust
//! implementation and through the TypeScript SDK and the contract playground's ABI codec (the Node oracle
//! `differential/oracle.mjs`). Divergences are grouped by the kind and the paths at which the two descriptions differ,
//! each group shown by its smallest input.
//!
//! Needs Node.js and the SDK built (`cd sdk/ts && npm ci && npm run build`). Run:
//!
//! ```sh
//! cargo run --release --manifest-path fuzz/Cargo.toml --example differential -- \
//!     [--cases <n>] [--seed <n>] [--kind <kind>]... [--no-vectors] [--out <file.json>]
//! ```
//!
//! `--cases` is the number of generated requests (default 20 000, spread evenly over the kinds), `--seed` makes the
//! run repeatable (default 1), `--kind` limits it to some kinds (`vmop`, `payload`, `borsh`, `tx`, `script`,
//! `witness`, `write`, `address`, `account`, `bip322`, `bits`, `pow`, `rewards`, `event`, `abi`, `abi_event`,
//! `merkle`, `params`), `--out` writes the groups as JSON. Exit status 0 when the two sides agree on everything, 1
//! when they diverged, 2 when the oracle failed or the options are wrong.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use ksana_fuzz::differential::gen::{generate, kinds, Rng, Seeds};
use ksana_fuzz::differential::{check_all, Divergence, Oracle, Request};
use serde_json::{json, Value};

/// Requests per line sent to the oracle.
const BATCH: usize = 200;

struct Options {
    cases: usize,
    seed: u64,
    kinds: Vec<&'static str>,
    vectors: bool,
    out: Option<PathBuf>,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        cases: 20_000,
        seed: 1,
        kinds: Vec::new(),
        vectors: true,
        out: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--cases" => {
                options.cases = value("--cases")?
                    .parse()
                    .map_err(|e| format!("--cases: {e}"))?;
            }
            "--seed" => {
                options.seed = value("--seed")?
                    .parse()
                    .map_err(|e| format!("--seed: {e}"))?;
            }
            "--kind" => {
                let kind = value("--kind")?;
                let known = kinds().iter().find(|k| **k == kind).ok_or(format!(
                    "unknown kind {kind}: one of {}",
                    kinds().join(", ")
                ))?;
                options.kinds.push(known);
            }
            "--no-vectors" => options.vectors = false,
            "--out" => options.out = Some(PathBuf::from(value("--out")?)),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if options.kinds.is_empty() {
        options.kinds = kinds().to_vec();
    }
    Ok(options)
}

/// How one kind fared.
#[derive(Default)]
struct Tally {
    agreed: usize,
    skipped: usize,
    diverged: usize,
}

/// The divergences of one signature.
struct Group {
    count: usize,
    smallest: Divergence,
    size: usize,
}

fn size(request: &Request) -> usize {
    request.to_json().to_string().len()
}

fn main() -> ExitCode {
    let options = match options() {
        Ok(options) => options,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let seeds = Seeds::load(&root);
    let splice = seeds.all_bytes();
    let mut oracle = match Oracle::start() {
        Ok(oracle) => oracle,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    eprintln!("oracle: {}", oracle.hello);

    let mut requests: Vec<Request> = Vec::new();
    if options.vectors {
        requests.extend(
            seeds
                .requests()
                .into_iter()
                .filter(|r| options.kinds.contains(&r.kind())),
        );
    }
    let vectors = requests.len();
    let mut rng = Rng::new(options.seed);
    for i in 0..options.cases {
        let kind = options.kinds[i % options.kinds.len()];
        requests.push(generate(&mut rng, kind, &seeds, &splice));
    }
    eprintln!(
        "{vectors} requests from the vectors, {} generated (seed {})",
        options.cases, options.seed
    );

    let mut tally: BTreeMap<&'static str, Tally> = BTreeMap::new();
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for (n, chunk) in requests.chunks(BATCH).enumerate() {
        let results = match check_all(&mut oracle, chunk) {
            Ok(results) => results,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        };
        for (request, result) in chunk.iter().zip(results) {
            let entry = tally.entry(request.kind()).or_default();
            match result {
                Ok(true) => entry.agreed += 1,
                Ok(false) => entry.skipped += 1,
                Err(divergence) => {
                    entry.diverged += 1;
                    let size = size(&divergence.request);
                    match groups.get_mut(&divergence.signature()) {
                        Some(group) => {
                            group.count += 1;
                            if size < group.size {
                                group.smallest = divergence;
                                group.size = size;
                            }
                        }
                        None => {
                            groups.insert(
                                divergence.signature(),
                                Group {
                                    count: 1,
                                    smallest: divergence,
                                    size,
                                },
                            );
                        }
                    }
                }
            }
        }
        if (n + 1) % 20 == 0 {
            eprintln!(
                "{} / {} requests, {} kinds of divergence",
                (n + 1) * BATCH,
                requests.len(),
                groups.len()
            );
        }
    }

    println!(
        "{:<10} {:>8} {:>8} {:>8}",
        "kind", "agreed", "skipped", "diverged"
    );
    for (kind, t) in &tally {
        println!(
            "{kind:<10} {:>8} {:>8} {:>8}",
            t.agreed, t.skipped, t.diverged
        );
    }
    for (signature, group) in &groups {
        println!("\n== {}x {signature}", group.count);
        println!("request: {}", group.smallest.request.to_json());
        println!("rust:    {}", group.smallest.rust);
        println!("ts:      {}", group.smallest.ts);
    }
    if let Some(out) = &options.out {
        let report = json!({
            "seed": options.seed,
            "cases": options.cases,
            "vectors": vectors,
            "oracle": oracle.hello,
            "kinds": tally
                .iter()
                .map(|(k, t)| (k.to_string(), json!({ "agreed": t.agreed, "skipped": t.skipped, "diverged": t.diverged })))
                .collect::<serde_json::Map<String, Value>>(),
            "divergences": groups
                .iter()
                .map(|(signature, g)| json!({ "signature": signature, "count": g.count, "smallest": g.smallest.to_json() }))
                .collect::<Vec<_>>(),
        });
        let text = serde_json::to_string_pretty(&report).unwrap_or_default();
        if let Err(e) = std::fs::write(out, text) {
            eprintln!("{}: {e}", out.display());
            return ExitCode::from(2);
        }
    }
    if groups.is_empty() {
        println!("\nno divergence");
        ExitCode::SUCCESS
    } else {
        println!("\n{} kinds of divergence", groups.len());
        ExitCode::from(1)
    }
}
