//! Test only: the wasm bytecode of the example contracts (`contracts/examples`, SPEC §9.5) and the test contracts
//! (`contracts/testing`).
//!
//! `build.rs` compiles each contract to `wasm32-unknown-unknown` with the dependencies locked by `contracts/Cargo.lock`
//! (release, compiled only as `cdylib`); they are exported here as byte constants. For each contract's methods and the
//! Borsh types of its arguments and return values see the docs of the contract crate
//! (`contracts/examples/*/src/lib.rs`, `contracts/testing/*/src/lib.rs`).

/// Fungible token, the reference KS-20 token (`contracts/examples/token`).
pub const TOKEN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/token.wasm"));

/// Fixed-supply token issued as a VM built-in asset (`contracts/examples/asset-token`).
pub const ASSET_TOKEN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/asset_token.wasm"));

/// Constant-product AMM (`contracts/examples/amm`): either side can be a token contract or a built-in asset.
pub const AMM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/amm.wasm"));

/// AMM pair factory (`contracts/examples/amm-factory`, creates pairs with `create_contract`).
pub const AMM_FACTORY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/amm_factory.wasm"));

/// Payout triggered by a BTC payment (`contracts/examples/btc-payment-trigger`).
pub const BTC_PAYMENT_TRIGGER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/btc_payment_trigger.wasm"));

/// Order-based exchange of BTC for KSA (`contracts/examples/ksa-exchange`, top-up, D-078).
pub const KSA_EXCHANGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ksa_exchange.wasm"));

/// A poll with one vote per account, the contract of the docs' tutorial "Your first contract" (`contracts/examples/poll`,
/// D-124).
pub const POLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/poll.wasm"));

/// Test contract exposing the host functions as methods (`contracts/testing/probe`).
pub const PROBE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/probe.wasm"));

/// Test contract taking KS-20 and KS-721 tokens by `transfer_and_call` (`contracts/testing/receiver`).
pub const RECEIVER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/receiver.wasm"));

/// Test contract: a KS-721 collection on the contract SDK's `ks721` module (`contracts/testing/collection`).
pub const COLLECTION: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/collection.wasm"));

/// All contracts: `(name, bytecode)`.
pub const ALL: [(&str, &[u8]); 10] = [
    ("token", TOKEN),
    ("asset-token", ASSET_TOKEN),
    ("amm", AMM),
    ("amm-factory", AMM_FACTORY),
    ("btc-payment-trigger", BTC_PAYMENT_TRIGGER),
    ("ksa-exchange", KSA_EXCHANGE),
    ("poll", POLL),
    ("probe", PROBE),
    ("receiver", RECEIVER),
    ("collection", COLLECTION),
];
