//! Determinism test (SPEC §14, M1 acceptance item 3; §15 "determinism test").
//!
//! A fixed block sequence (built-in asset transfers, path C, contract deployment and calls, trap, out of gas, failed
//! transfers, empty blocks, plus the attached funds, catchable calls, delegate calls, contract creation, asset burning
//! and runtime stack metering of D-073, and the issuer capabilities of D-121) is executed and committed block by block
//! on RocksDB + JMT. The parent process
//! executes it once itself, then starts two child processes (re-running this test binary) that each execute it once,
//! and compares the `state_root` and the receipt digest block by block, requiring byte-for-byte equality. The processes
//! are configured differently:
//!
//! - parent process: default engine (parallel compilation), cold module cache;
//! - child `serial`: wasmtime parallel compilation off, `RAYON_NUM_THREADS=1`;
//! - child `parallel`: parallel compilation on, `RAYON_NUM_THREADS=4`; within the process, two threads first share one
//!   executor (one module cache) and execute concurrently, then one more sequential run uses the warmed-up cache; all
//!   three results must be identical.
//!
//! The two children run at the same time, started in the opposite order from the parent. The parent also checks that
//! the success or failure of every receipt matches the expectation, confirming that this set of entries really covers
//! the success path and the various failure paths; and it compares each block's root with the fixed values so that the
//! results can also be checked across machines and architectures (SPEC §15: x86_64 and aarch64).
//!
//! This file is a `harness = false` test: `main` tells parent and child apart by the `KSANA_DETERMINISM_CHILD`
//! environment variable.

mod common;

use std::collections::BTreeMap;
use std::process::{Command, ExitCode, Output, Stdio};
use std::sync::Arc;

use common::*;
use ksana_crypto::entries_root;
use ksana_execution::builtin::{asset_id, contract_id};
use ksana_execution::{BlockEnv, Executor, GenesisConfig, OpStatus, Receipt};
use ksana_primitives::asset::{CAPABILITIES, CAP_MINT, CAP_PAUSE};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, BlockHeader, BtcAnchor, Entry, Funds, DEVNET};
use ksana_state::StateDb;
use ksana_vm::{AssetMetaInput, Vm};

const CHILD_ENV: &str = "KSANA_DETERMINISM_CHILD";

/// Fixed bytecode of the example token and AMM contracts (one build of `contracts/examples`). A contract's wasm build
/// output varies with the absolute path of its source (D-031), and the fixed roots must be based on fixed inputs, so
/// the version built on the spot by `ksana-test-contracts` is not used here.
const TOKEN: &[u8] = include_bytes!("data/token.wasm");
const AMM: &[u8] = include_bytes!("data/amm.wasm");
/// Fixed bytecode of the test contract probe (`contracts/testing/probe`): covers the attached funds, catchable calls,
/// delegate calls, contract creation, asset burning and runtime stack metering of D-073 (blocks 11-13) and the asset
/// management functions of D-121 (block 14).
const PROBE: &[u8] = include_bytes!("data/probe.wasm");

/// Fixed values of each block's `state_root`: different machines and architectures (CI's x86_64 and aarch64) must
/// produce the same roots. The real contracts in it use the fixed bytecode in `tests/data/` (D-031).
/// They need updating when the scenario, the execution rules or the example contracts change
/// (updated once for M7's block header v2, D-063; again for M8's VmOp version 2 and attached funds and the AMM rebuilt
/// against the new `call_contract` signature, when blocks 11-13 were added: the probe contract covers D-073's new host
/// functions and runtime stack metering; again when M9's block rewards gave every block a lock record and made the KSA
/// total supply grow block by block, D-074; again when failed path-C ops started consuming the nonce, revision of
/// D-018; again when the arrears field was removed from the account state, D-078; again when sponsored fees became
/// recorded by acceptance position and block rewards paid at unlock time, with empty blocks leaving the state root
/// unchanged, D-079; again when accounts became BIP86-tweaked Taproot output keys, which changed the bytes of every
/// account in the scenario, D-092; again when assets gained a symbol, issuer capabilities and a pause, which changed
/// KSA's record in the genesis state, and block 14 was added for the capabilities, D-121):
/// `KSANA_DETERMINISM_CHILD=serial target/debug/deps/determinism-<hash>` prints each block's root (3rd column).
const GOLDEN_STATE_ROOTS: [&str; 14] = [
    "88af4448df018223354d29740a8decc192d864aa9706f6bf1b1cd88b5bece66a",
    "516ad87c8b1ee8c69c735ea7c96c0c1be87d848e7ca95f33223770106406cd9e",
    "e1ee225442e345f7194f09ec2ae641b88f5e00eb2ccc7f36f2cf015d2a73a031",
    "e1ee225442e345f7194f09ec2ae641b88f5e00eb2ccc7f36f2cf015d2a73a031",
    "6a99694a52632b8c46408082cf377d800c734356d4caab30e24c316da1d1a8a6",
    "84d55724b8ccaeea390b878732b361dd2a88bdf3a61183b7b1cdb1f8c5601636",
    "3a231c00688f8c5e57af0b9b14aaff59d6fde766f37a34c54dedbc399895fb25",
    "e46cc6dfea3278bb3760d7db52549141609bcc48d7307ccfdfaa5cd2261b4400",
    "5b302a2771585e92c92dc39b3271c54c4fa2e478fa465be05189bd0ab905e3db",
    "1b058a02ad5619470efdd559c601df8c1fec6b72be18f54455f7d521ee76a481",
    "70f9349416db9572ee1229cc1eadc25379a16033db33667a83ccbedfaf44a7d9",
    "76036abf666374a76277228c701241f9137f61987bba05535e9dbc25e2d814f5",
    "04eee20dd719524224ed765e734003c7ecd33fd9b610956b7ac616e8c9728d00",
    "14d8969b030b6e2d364b0258dcfbe5e062ee24d3b9f8d89bf7bb1ec20db83db3",
];
const LINE_PREFIX: &str = "KSANA_BLOCK ";
const GAS: u64 = 5_000_000;
/// max_gas of a built-in asset transfer (fixed cost 10 000, ABI §4).
const TRANSFER_GAS: u64 = 100_000;

/// Per block: `(state_root, receipt digest)`.
type Trace = Vec<([u8; 32], [u8; 32])>;

/// The fixed block sequence and the expected success or failure of every entry.
struct Scenario {
    blocks: Vec<Vec<Entry>>,
    expect_success: Vec<Vec<bool>>,
}

/// Tracks nonces per account, builds entries and records the expected success or failure.
#[derive(Default)]
struct Builder {
    nonces: BTreeMap<[u8; 32], u64>,
    blocks: Vec<Vec<Entry>>,
    expect: Vec<Vec<bool>>,
}

impl Builder {
    fn nonce(&self, u: &User) -> u64 {
        self.nonces.get(&u.account).copied().unwrap_or(0)
    }

    fn bump(&mut self, u: &User) {
        *self.nonces.entry(u.account).or_insert(0) += 1;
    }

    fn start_block(&mut self) {
        self.blocks.push(Vec::new());
        self.expect.push(Vec::new());
    }

    fn push(&mut self, entry: Entry, ok: bool) {
        self.blocks.last_mut().unwrap().push(entry);
        self.expect.last_mut().unwrap().push(ok);
    }

    /// Path B: when the block is valid the nonce is always incremented (even if the action fails).
    fn sponsored(&mut self, u: &User, fee: u128, max_gas: u64, action: Action, ok: bool) {
        let entry = sponsored(u, self.nonce(u), fee, max_gas, action);
        self.bump(u);
        self.push(entry, ok);
    }

    /// Path C: with a correct signature and nonce the nonce is always incremented, even if the action fails (revision
    /// of D-018).
    fn forced(&mut self, u: &User, max_gas: u64, action: Action, ok: bool) {
        let entry = forced(u, self.nonce(u), max_gas, action);
        self.bump(u);
        self.push(entry, ok);
    }

    fn finish(self) -> Scenario {
        Scenario {
            blocks: self.blocks,
            expect_success: self.expect,
        }
    }
}

fn users() -> [User; 4] {
    ["a", "b", "c", "d"].map(user)
}

fn allocations() -> Vec<([u8; 32], u128)> {
    users()
        .iter()
        .enumerate()
        .map(|(i, u)| (u.account, 1_000_000 + i as u128))
        .collect()
}

fn call(contract: [u8; 32], method: &str) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: vec![],
        funds: Vec::new(),
    }
}

fn call_with<T: borsh::BorshSerialize>(contract: [u8; 32], method: &str, args: &T) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: borsh::to_vec(args).unwrap(),
        funds: Vec::new(),
    }
}

fn deploy_with<T: borsh::BorshSerialize>(code: &[u8], init_args: &T) -> Action {
    Action::Deploy {
        code: code.to_vec(),
        init_args: borsh::to_vec(init_args).unwrap(),
        funds: Vec::new(),
    }
}

fn ksa(amount: u128) -> Funds {
    Funds {
        asset: [0; 32],
        amount,
    }
}

/// A call with attached funds (D-073).
fn call_funds<T: borsh::BorshSerialize>(
    contract: [u8; 32],
    method: &str,
    args: &T,
    funds: Vec<Funds>,
) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args: borsh::to_vec(args).unwrap(),
        funds,
    }
}

fn scenario() -> Scenario {
    let [a, b, c, d] = users();
    let mut s = Builder::default();

    // 1: path C first, then path-B transfers
    s.start_block();
    s.forced(&c, 100_000, transfer(d.account, 50), true);
    s.forced(&d, 100_000, transfer(a.account, 9_000_000), false);
    s.sponsored(&a, 10, GAS, transfer(b.account, 1_000), true);
    s.sponsored(&b, 3, GAS, transfer(c.account, 500), true);
    s.sponsored(&d, 1, GAS, transfer(a.account, 1), true);

    // 2: deployments (one of them contains floating-point instructions and is rejected)
    s.start_block();
    let writer = writer_contract();
    let trap = trap_after_write_contract();
    let spin = spin_after_write_contract();
    let writer_id = contract_id(&a.account, s.nonce(&a), &sha256(&writer));
    s.sponsored(&a, 5, GAS, deploy(writer.clone()), true);
    let trap_id = contract_id(&b.account, s.nonce(&b), &sha256(&trap));
    s.sponsored(&b, 5, GAS, deploy(trap), true);
    let spin_id = contract_id(&c.account, s.nonce(&c), &sha256(&spin));
    s.sponsored(&c, 5, GAS, deploy(spin), true);
    s.sponsored(&d, 5, GAS, deploy(float_contract()), false);

    // 3: successful call, trap, out of gas, insufficient balance; path-C transfer
    s.start_block();
    s.forced(&b, 100_000, transfer(a.account, 7), true);
    s.sponsored(&d, 2, GAS, call(writer_id, "go"), true);
    s.sponsored(&a, 2, GAS, call(trap_id, "go"), false);
    s.sponsored(&b, 2, 300_000, call(spin_id, "go"), false);
    s.sponsored(&c, 2, GAS, transfer(d.account, 50_000_000), false);

    // 4: empty block (writes no state, the state root equals the parent's, D-079)
    s.start_block();

    // 5: the same code deployed again by another account (different contract ID); circular transfers
    s.start_block();
    s.forced(&a, 100_000, transfer(b.account, 1), true);
    s.sponsored(&b, 4, GAS, deploy(writer), true);
    for (from, to) in [(&a, &b), (&b, &c), (&c, &d), (&d, &a)] {
        s.sponsored(from, 1, GAS, transfer(to.account, 333), true);
    }

    // 6: many small transfers, growing the state tree
    s.start_block();
    let all = users();
    for i in 0..24u128 {
        let from = &all[(i % 4) as usize];
        let to = &all[((i + 1 + i / 4) % 4) as usize];
        s.sponsored(
            from,
            1 + i % 3,
            TRANSFER_GAS,
            transfer(to.account, 10 + i),
            true,
        );
    }

    // 7: real contracts (contracts/examples): two tokens and an AMM
    s.start_block();
    let token_a = contract_id(&a.account, s.nonce(&a), &sha256(TOKEN));
    s.sponsored(
        &a,
        3,
        GAS,
        deploy_with(
            TOKEN,
            &("Alpha".to_string(), "A".to_string(), 6u8, 10_000_000u128),
        ),
        true,
    );
    let token_b = contract_id(&b.account, s.nonce(&b), &sha256(TOKEN));
    s.sponsored(
        &b,
        3,
        GAS,
        deploy_with(
            TOKEN,
            &("Beta".to_string(), "B".to_string(), 8u8, 50_000_000u128),
        ),
        true,
    );
    let amm = contract_id(&c.account, s.nonce(&c), &sha256(AMM));
    s.sponsored(&c, 3, GAS, deploy_with(AMM, &(token_a, token_b)), true);

    // 8: distribute tokens, approve, add liquidity (AMM → token contract-to-contract calls)
    s.start_block();
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(token_a, "transfer", &(c.account, 1_000_000u128)),
        true,
    );
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(token_a, "transfer", &(d.account, 50_000u128)),
        true,
    );
    s.sponsored(
        &b,
        1,
        GAS,
        call_with(token_b, "transfer", &(c.account, 4_000_000u128)),
        true,
    );
    s.sponsored(
        &c,
        1,
        GAS,
        call_with(token_a, "approve", &(amm, 1_000_000u128)),
        true,
    );
    s.sponsored(
        &c,
        1,
        GAS,
        call_with(token_b, "approve", &(amm, 4_000_000u128)),
        true,
    );
    s.sponsored(
        &c,
        1,
        GAS,
        call_with(amm, "add_liquidity", &(1_000_000u128, 4_000_000u128)),
        true,
    );

    // 9: swap; slippage protection failure; insufficient token balance; unapproved transfer_from; contract call with
    // insufficient gas
    s.start_block();
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(token_a, "approve", &(amm, 50_000u128)),
        true,
    );
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(amm, "swap", &(token_a, 20_000u128, 1u128)),
        true,
    );
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(amm, "swap", &(token_a, 10_000u128, 1_000_000u128)),
        false,
    );
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(token_b, "transfer", &(a.account, 1_000_000_000u128)),
        false,
    );
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(token_b, "transfer_from", &(c.account, a.account, 1u128)),
        false,
    );
    s.sponsored(
        &a,
        1,
        30_000,
        call_with(token_a, "transfer", &(b.account, 1u128)),
        false,
    );

    // 10: remove part of the liquidity; swap once more in the other direction
    s.start_block();
    s.sponsored(
        &c,
        1,
        GAS,
        call_with(amm, "remove_liquidity", &500_000u128),
        true,
    );
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(token_b, "approve", &(amm, 10_000u128)),
        true,
    );
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(amm, "swap", &(token_b, 10_000u128, 1u128)),
        true,
    );

    // 11: the D-073 contract model: deploy two probes, one with KSA attached
    s.start_block();
    let p = contract_id(&a.account, s.nonce(&a), &sha256(PROBE));
    s.sponsored(
        &a,
        1,
        GAS,
        Action::Deploy {
            code: PROBE.to_vec(),
            init_args: borsh::to_vec(&b"ok".to_vec()).unwrap(),
            funds: vec![ksa(1_000)],
        },
        true,
    );
    let q = contract_id(&b.account, s.nonce(&b), &sha256(PROBE));
    s.sponsored(&b, 1, GAS, deploy_with(PROBE, &b"ok".to_vec()), true);

    // 12: catchable calls (revert, sub-call out of gas), contract creation, burning, delegate call,
    // contract-to-contract attached funds, recursion and stack metering
    s.start_block();
    let no_funds: Vec<Funds> = Vec::new();
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(
            p,
            "try_forward",
            &(
                q,
                "revert".to_string(),
                Vec::<u8>::new(),
                vec![ksa(10)],
                0u64,
            ),
        ),
        true,
    );
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(
            p,
            "try_forward",
            &(
                q,
                "spin".to_string(),
                Vec::<u8>::new(),
                no_funds,
                100_000u64,
            ),
        ),
        true,
    );
    s.sponsored(
        &b,
        1,
        GAS,
        call_with(
            p,
            "create",
            &(
                sha256(PROBE),
                [1u8; 32],
                borsh::to_vec(&b"ok".to_vec()).unwrap(),
                vec![ksa(5)],
            ),
        ),
        true,
    );
    s.sponsored(
        &c,
        1,
        GAS,
        call_with(p, "burn", &([0u8; 32], 100u128)),
        true,
    );
    s.sponsored(&d, 1, GAS, call_with(q, "recurse", &200u32), true);
    s.sponsored(&d, 1, GAS, call_with(q, "recurse", &1_000_000u32), false);
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(
            p,
            "delegate",
            &(
                q,
                "set".to_string(),
                borsh::to_vec(&(b"k".to_vec(), b"v".to_vec())).unwrap(),
            ),
        ),
        true,
    );
    s.sponsored(
        &b,
        1,
        GAS,
        call_with(
            p,
            "forward",
            &(q, "funds".to_string(), Vec::<u8>::new(), vec![ksa(7)]),
        ),
        true,
    );

    // 13: core::fmt and dynamic dispatch; path C with attached funds
    s.start_block();
    s.forced(&c, 100_000, call_funds(p, "funds", &(), vec![ksa(3)]), true);
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(p, "format", &(12_345u64, "x".to_string())),
        true,
    );
    s.sponsored(&a, 1, GAS, call_with(p, "dyn_sum", &1_000u32), true);

    // 14: the issuer capabilities of D-121: p creates an asset with all four and an allowlist, freezes, pauses and
    // renounces; refused movements fail their ops
    s.start_block();
    let sub_id = [2u8; 32];
    let asset = asset_id(&p, &sub_id);
    let meta = AssetMetaInput {
        name: "Dollar".into(),
        symbol: "USDX".into(),
        decimals: 6,
    };
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(
            p,
            "create_asset",
            &(sub_id, meta.clone(), CAPABILITIES, p, 1_000u128),
        ),
        true,
    );
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(p, "allow", &(sub_id, a.account, true)),
        true,
    );
    s.sponsored(
        &a,
        1,
        GAS,
        call_with(p, "send", &(asset, a.account, 300u128)),
        true,
    );
    let send_asset = |to: &User, amount: u128| Action::AssetTransfer {
        asset,
        to: to.account,
        amount,
    };
    s.sponsored(&a, 1, TRANSFER_GAS, send_asset(&b, 10), false);
    s.sponsored(
        &b,
        1,
        GAS,
        call_with(p, "allow", &(sub_id, b.account, true)),
        true,
    );
    s.sponsored(
        &b,
        1,
        GAS,
        call_with(p, "freeze", &(sub_id, a.account, true)),
        true,
    );
    s.sponsored(&a, 1, TRANSFER_GAS, send_asset(&b, 10), false);
    s.sponsored(&c, 1, GAS, call_with(p, "pause", &(sub_id, true)), true);
    s.sponsored(
        &c,
        1,
        GAS,
        call_with(p, "send", &(asset, b.account, 5u128)),
        true,
    );
    s.sponsored(&b, 1, TRANSFER_GAS, send_asset(&a, 1), false);
    s.sponsored(
        &c,
        1,
        GAS,
        call_with(p, "renounce", &(sub_id, CAP_PAUSE | CAP_MINT)),
        true,
    );
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(p, "freeze", &(sub_id, a.account, false)),
        true,
    );
    s.sponsored(&a, 1, TRANSFER_GAS, send_asset(&b, 10), true);
    s.sponsored(
        &d,
        1,
        GAS,
        call_with(p, "issue_meta", &(sub_id, meta, b.account, 1u128)),
        false,
    );
    s.finish()
}

/// Executes the whole block sequence on a fresh RocksDB + JMT, committing block by block; block headers are built in
/// the real format and the next block's parent hash is their hash.
fn replay(exec: &Executor, blocks: &[Vec<Entry>]) -> Result<(Trace, Vec<Vec<Receipt>>), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut db = StateDb::open(dir.path()).map_err(|e| e.to_string())?;
    let genesis = GenesisConfig::devnet(allocations())
        .writes()
        .map_err(|e| e.to_string())?;
    let genesis_root = db.commit(0, &genesis).map_err(|e| e.to_string())?;
    let producer = user("producer").account;
    let btc_anchor = BtcAnchor {
        height: 200,
        hash: [9; 32],
    };
    let mut parent = BlockHeader {
        version: 2,
        height: 0,
        parents: vec![],
        btc_anchor: btc_anchor.clone(),
        entries_root: entries_root(&[]).map_err(|e| e.to_string())?,
        state_root: genesis_root,
        producer,
        timestamp: 1_700_000_000,
        bits: 0x207f_ffff,
        nonce: 0,
    };
    let mut trace = Trace::new();
    let mut all_receipts = Vec::new();
    for entries in blocks {
        let parent_hash = parent.hash();
        let env = BlockEnv {
            height: parent.height + 1,
            btc_anchor: btc_anchor.clone(),
            producer,
            timestamp: parent.timestamp + DEVNET.target_block_time_secs,
            rules: DEVNET.rules(parent.height + 1),
            // The scenario has no unlock entries; the test view only answers the first input a path-C op is bound to
            // (determined by the transaction ID, D-086)
            unlockable: Vec::new(),
        };
        let view = db.view().map_err(|e| e.to_string())?;
        let out = exec
            .execute_block(Arc::new(view), Arc::new(TestBtc::default()), &env, entries)
            .map_err(|e| format!("block {} failed to execute: {e}", env.height))?;
        let state_root = db
            .commit(env.height, &out.writes)
            .map_err(|e| e.to_string())?;
        trace.push((state_root, sha256(format!("{:?}", out.receipts).as_bytes())));
        all_receipts.push(out.receipts);
        parent = BlockHeader {
            version: 2,
            height: env.height,
            parents: vec![parent_hash],
            btc_anchor: btc_anchor.clone(),
            entries_root: entries_root(entries).map_err(|e| e.to_string())?,
            state_root,
            producer,
            timestamp: env.timestamp,
            bits: parent.bits,
            nonce: 0,
        };
    }
    Ok((trace, all_receipts))
}

fn format_trace(trace: &Trace) -> String {
    trace
        .iter()
        .enumerate()
        .map(|(i, (root, receipts))| {
            format!(
                "{LINE_PREFIX}{} {} {}\n",
                i + 1,
                hex::encode(root),
                hex::encode(receipts)
            )
        })
        .collect()
}

fn parse_trace(stdout: &[u8]) -> Result<Trace, String> {
    let text = String::from_utf8(stdout.to_vec()).map_err(|e| e.to_string())?;
    let mut trace = Trace::new();
    for line in text.lines().filter_map(|l| l.strip_prefix(LINE_PREFIX)) {
        let parts: Vec<&str> = line.split(' ').collect();
        let [height, root, receipts] = parts[..] else {
            return Err(format!("cannot parse child output line: {line}"));
        };
        if height != (trace.len() + 1).to_string() {
            return Err(format!(
                "block heights in the child output are not consecutive: {line}"
            ));
        }
        let decode = |s: &str| -> Result<[u8; 32], String> {
            hex::decode(s)
                .map_err(|e| e.to_string())?
                .try_into()
                .map_err(|_| format!("length is not 32 bytes: {s}"))
        };
        trace.push((decode(root)?, decode(receipts)?));
    }
    Ok(trace)
}

fn compare(name: &str, expected: &Trace, got: &Trace) -> Result<(), String> {
    if expected.len() != got.len() {
        return Err(format!(
            "{name}: block count {} differs from the parent's {}",
            got.len(),
            expected.len()
        ));
    }
    for (i, (e, g)) in expected.iter().zip(got).enumerate() {
        if e.0 != g.0 {
            return Err(format!(
                "{name}: state_root of block {} differs: {} != {}",
                i + 1,
                hex::encode(g.0),
                hex::encode(e.0)
            ));
        }
        if e.1 != g.1 {
            return Err(format!("{name}: receipts of block {} differ", i + 1));
        }
    }
    Ok(())
}

/// This set of entries really covers the expected success and failure paths, and blocks with entries all changed the
/// state while empty blocks did not (D-079).
fn check_scenario(
    scenario: &Scenario,
    trace: &Trace,
    receipts: &[Vec<Receipt>],
) -> Result<(), String> {
    for (height, (want, got)) in scenario.expect_success.iter().zip(receipts).enumerate() {
        let got: Vec<bool> = got.iter().map(|r| r.status == OpStatus::Success).collect();
        if *want != got {
            return Err(format!(
                "success/failure of block {}'s receipts {got:?} does not match the expectation {want:?}: {:?}",
                height + 1,
                receipts[height]
            ));
        }
    }
    for (i, window) in trace.windows(2).enumerate() {
        let empty = scenario.blocks.get(i + 1).is_some_and(Vec::is_empty);
        if (window[0].0 == window[1].0) != empty {
            return Err(format!(
                "state_root of block {} is {}the same as the parent's: a block with entries should change the state, an empty block should not (D-079)",
                i + 2,
                if empty { "not " } else { "" }
            ));
        }
    }
    Ok(())
}

fn child(mode: &str) -> Result<(), String> {
    let blocks = scenario().blocks;
    let trace = match mode {
        "serial" => {
            let vm = Vm::with_parallel_compilation(&DEVNET, false).map_err(|e| e.to_string())?;
            replay(&Executor::with_vm(DEVNET, vm), &blocks)?.0
        }
        "parallel" => {
            let vm = Vm::with_parallel_compilation(&DEVNET, true).map_err(|e| e.to_string())?;
            let exec = Arc::new(Executor::with_vm(DEVNET, vm));
            let blocks = Arc::new(blocks);
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let (exec, blocks) = (Arc::clone(&exec), Arc::clone(&blocks));
                    std::thread::spawn(move || replay(&exec, &blocks).map(|r| r.0))
                })
                .collect();
            let mut traces = Vec::new();
            for handle in handles {
                traces.push(
                    handle
                        .join()
                        .map_err(|_| "execution thread exited abnormally".to_string())??,
                );
            }
            // The module cache is warmed up; run once more sequentially
            traces.push(replay(&exec, &blocks)?.0);
            let first = traces.remove(0);
            for (i, other) in traces.iter().enumerate() {
                compare(
                    &format!("run {} within the parallel process", i + 2),
                    &first,
                    other,
                )?;
            }
            first
        }
        other => return Err(format!("unknown child mode: {other}")),
    };
    print!("{}", format_trace(&trace));
    Ok(())
}

fn spawn_child(mode: &str, rayon_threads: &str) -> Result<std::process::Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Command::new(exe)
        .env(CHILD_ENV, mode)
        .env("RAYON_NUM_THREADS", rayon_threads)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start child {mode}: {e}"))
}

fn child_trace(mode: &str, output: std::io::Result<Output>) -> Result<Trace, String> {
    let output = output.map_err(|e| format!("waiting for child {mode} failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "child {mode} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse_trace(&output.stdout)
}

fn parent() -> Result<(), String> {
    // The children are started first and run concurrently with the parent's own execution
    let parallel = spawn_child("parallel", "4")?;
    let serial = spawn_child("serial", "1")?;

    let scenario = scenario();
    let exec = Executor::new(DEVNET).map_err(|e| e.to_string())?;
    let (trace, receipts) = replay(&exec, &scenario.blocks)?;
    check_scenario(&scenario, &trace, &receipts)?;
    let roots: Vec<String> = trace.iter().map(|(root, _)| hex::encode(root)).collect();
    if roots != GOLDEN_STATE_ROOTS {
        return Err(format!(
            "state_root differs from the fixed values ({}):\n{}",
            std::env::consts::ARCH,
            roots.join("\n")
        ));
    }

    for (mode, child) in [("serial", serial), ("parallel", parallel)] {
        let got = child_trace(mode, child.wait_with_output())?;
        compare(mode, &trace, &got)?;
    }
    println!(
        "determinism test passed: {} blocks executed in 3 processes ({} engine configurations), state_root and receipts identical byte for byte; final state_root {}",
        trace.len(),
        3,
        hex::encode(trace.last().map(|t| t.0).unwrap_or_default())
    );
    Ok(())
}

fn main() -> ExitCode {
    let result = match std::env::var(CHILD_ENV) {
        Ok(mode) => child(&mode),
        Err(_) => parent(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("determinism test failed: {e}");
            ExitCode::FAILURE
        }
    }
}
