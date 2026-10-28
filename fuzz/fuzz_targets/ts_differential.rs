//! The differential test of the TypeScript SDK (D-133): the first byte picks what is compared, the rest is its input
//! (`Request::from_fuzz`); the Rust implementation and the SDK or the contract playground's ABI codec, asked through
//! the Node oracle `differential/oracle.mjs`, must describe it the same way. Needs Node.js and the SDK built
//! (`cd sdk/ts && npm ci && npm run build`).

#![no_main]

use std::sync::{Mutex, OnceLock};

use ksana_fuzz::differential::{check, Oracle, Request};
use libfuzzer_sys::fuzz_target;

/// One oracle for the whole run; it exits when this process does (its standard input closes).
fn oracle() -> &'static Mutex<Oracle> {
    static ORACLE: OnceLock<Mutex<Oracle>> = OnceLock::new();
    ORACLE.get_or_init(|| {
        Mutex::new(Oracle::start().unwrap_or_else(|e| panic!("the oracle did not start: {e}")))
    })
}

fuzz_target!(|data: &[u8]| {
    let Some(request) = Request::from_fuzz(data) else {
        return;
    };
    let mut oracle = oracle().lock().unwrap_or_else(|e| e.into_inner());
    match check(&mut oracle, &request) {
        Ok(Ok(_)) => {}
        Ok(Err(divergence)) => panic!(
            "the Rust implementation and the TypeScript one diverge:\n{:#}",
            divergence.to_json()
        ),
        Err(e) => panic!("{e}"),
    }
});
