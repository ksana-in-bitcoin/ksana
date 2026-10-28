//! The differential test of the TypeScript implementations against the Rust ones (D-133).
//!
//! The TypeScript SDK (`sdk/ts`) mirrors the Rust code that decides what bytes mean: the consensus types and their
//! Borsh codec, the compact payloads, BTC transactions and the KSN1 carriers in them, accounts and addresses, BIP322,
//! the PoW target, the reward schedule, the token standards' events; the contract playground's ABI codec
//! (`playground/src/abi/codec.ts`, which the testnet points service uses too) mirrors the Borsh decoding of the contract
//! SDK. A web page, the playground or the points service that reads bytes differently from the node shows, signs or
//! credits something the chain does not have.
//!
//! Each [`Request`] goes to the Rust implementation ([`describe`]) and, through the Node oracle
//! `fuzz/differential/oracle.mjs` ([`Oracle`]), to the TypeScript one. Each side describes what it made of the input
//! as JSON: refused, or the decoded value in the D-007 JSON form with what is derived from it (hashes, signature
//! verdicts, the carrier and payload of a transaction). The two descriptions must be equal; a difference is a
//! [`Divergence`].
//!
//! A few inputs are outside what the two sides must agree on, and [`describe`] leaves them out (returns `None`):
//! a BTC transaction without inputs (consensus forbids it in a block, and rust-bitcoin reads a segwit-marked one that
//! Bitcoin Core and the SDK refuse), and an event longer than the VM lets a contract emit (`MAX_EVENT_BYTES`).

mod abi;
pub mod gen;
mod oracle;
mod request;
mod rust;

pub use abi::{ABI_EVENTS, ABI_TYPES};
pub use oracle::{Oracle, OracleError};
pub use request::{BorshType, BtcNet, Request, Signing, FIRST_INPUT};
pub use rust::{describe, MAX_BORSH_BYTES, WRITE_KEY};

use serde_json::Value;

/// The two sides described one input differently.
#[derive(Clone, Debug)]
pub struct Divergence {
    /// The input.
    pub request: Request,
    /// What the Rust implementation made of it.
    pub rust: Value,
    /// What the TypeScript implementation made of it.
    pub ts: Value,
}

impl Divergence {
    /// The paths at which the two descriptions differ (`$.value.action`, `$.data`), at most a few: what groups
    /// divergences of the same cause.
    pub fn paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        diff_paths(&self.rust, &self.ts, "$", &mut out);
        out
    }

    /// A one-line key for grouping: the kind and the differing paths.
    pub fn signature(&self) -> String {
        format!("{} {}", self.request.kind(), self.paths().join(" "))
    }

    /// The divergence as JSON: the request as the oracle got it and both descriptions.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "request": self.request.to_json(),
            "rust": self.rust,
            "ts": self.ts,
            "paths": self.paths(),
        })
    }
}

/// The most paths [`Divergence::paths`] lists.
const MAX_PATHS: usize = 4;

fn diff_paths(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    if a == b || out.len() >= MAX_PATHS {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for key in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
                let (va, vb) = (
                    x.get(key).unwrap_or(&Value::Null),
                    y.get(key).unwrap_or(&Value::Null),
                );
                diff_paths(va, vb, &format!("{path}.{key}"), out);
            }
        }
        // Arrays of the same length are compared item by item; otherwise the array as a whole differs
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (va, vb)) in x.iter().zip(y).enumerate() {
                diff_paths(va, vb, &format!("{path}[{i}]"), out);
            }
        }
        _ => out.push(path.to_string()),
    }
}

/// Runs one request through both sides: `Ok(true)` when they agree, `Ok(false)` when the request is outside what
/// they must agree on ([`describe`]), a [`Divergence`] otherwise.
pub fn check(
    oracle: &mut Oracle,
    request: &Request,
) -> Result<Result<bool, Divergence>, OracleError> {
    let Some(rust) = describe(request) else {
        return Ok(Ok(false));
    };
    let ts = oracle.ask(&request.to_json())?;
    Ok(divergence(request, rust, ts).map_or(Ok(true), Err))
}

/// Runs a batch of requests through both sides; the result of each in order, as [`check`] gives it.
pub fn check_all(
    oracle: &mut Oracle,
    requests: &[Request],
) -> Result<Vec<Result<bool, Divergence>>, OracleError> {
    let described: Vec<Option<Value>> = requests.iter().map(describe).collect();
    let asked: Vec<Value> = requests
        .iter()
        .zip(&described)
        .filter(|(_, rust)| rust.is_some())
        .map(|(request, _)| request.to_json())
        .collect();
    let mut answers = oracle.ask_all(&asked)?.into_iter();
    let mut out = Vec::with_capacity(requests.len());
    for (request, rust) in requests.iter().zip(described) {
        let Some(rust) = rust else {
            out.push(Ok(false));
            continue;
        };
        let ts = answers.next().ok_or(OracleError::Protocol(
            "the oracle answered fewer requests than it was asked".into(),
        ))?;
        out.push(divergence(request, rust, ts).map_or(Ok(true), Err));
    }
    Ok(out)
}

/// The [`Divergence`] of the two descriptions, `None` when they are equal.
fn divergence(request: &Request, rust: Value, ts: Value) -> Option<Divergence> {
    (rust != ts).then(|| Divergence {
        request: request.clone(),
        rust,
        ts,
    })
}
