//! Execution-layer test utilities: keys, signed ops, a configurable BTC view, block-by-block execution on an in-memory
//! state.

#![allow(dead_code)]

pub mod probe;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ksana_crypto::{account_of, keypair_from_secret, sign_native_vmop, sign_vmop, Keypair};
use ksana_execution::keys::{self, AccountState, LockedFee, UnlockCursor};
use ksana_execution::{
    BlockEnv, BlockError, BlockOutcome, BtcView, Executor, GenesisConfig, MemState, MergedBlock,
    UnlockItem,
};
use ksana_primitives::hash::sha256;
use ksana_primitives::{
    Action, BtcAnchor, Entry, FeeMode, OutPoint, Params, SigScheme, VmOp, DEVNET, VMOP_VERSION,
};

pub const KSA: [u8; 32] = [0u8; 32];

/// devnet params without block rewards: the fee and unlock tests only look at sponsored fees (block rewards are in
/// rewards.rs, D-074).
pub const NO_REWARD: Params = Params {
    block_reward: 0,
    ..DEVNET
};

pub struct User {
    pub kp: Keypair,
    pub account: [u8; 32],
}

pub fn user(seed: &str) -> User {
    let kp = keypair_from_secret(&sha256(seed.as_bytes())).unwrap();
    let account = account_of(&kp);
    User { kp, account }
}

/// A signed path-B op. Path-C ops use [`native_op`] (the signature is bound to the carrying transaction's first input,
/// D-086).
pub fn op(u: &User, nonce: u64, fee: FeeMode, max_gas: u64, action: Action) -> VmOp {
    let mut op = unsigned(u, nonce, fee, max_gas, action);
    op.sig = sign_vmop(&op, &u.kp, &[0u8; 32]).unwrap();
    op
}

/// A path-C op carried by transaction `btc_txid`: the signature is bound to the first input the test view gives this
/// transaction ([`test_first_input`]).
pub fn native_op(u: &User, nonce: u64, btc_txid: [u8; 32], max_gas: u64, action: Action) -> VmOp {
    let mut op = unsigned(u, nonce, FeeMode::BtcNative, max_gas, action);
    op.sig = sign_native_vmop(&op, &test_first_input(&btc_txid), &u.kp, &[0u8; 32]).unwrap();
    op
}

/// An unsigned op (`sig` is a placeholder).
pub fn unsigned(u: &User, nonce: u64, fee: FeeMode, max_gas: u64, action: Action) -> VmOp {
    VmOp {
        version: VMOP_VERSION,
        chain_id: DEVNET.chain_id,
        account: u.account,
        nonce,
        fee,
        max_gas,
        action,
        sig: SigScheme::Bip340([0; 64]),
    }
}

/// The first input of native transaction `btc_txid` in the test view (determined by the transaction ID); [`TestBtc`]
/// answers with it unless registered otherwise.
pub fn test_first_input(btc_txid: &[u8; 32]) -> OutPoint {
    OutPoint {
        txid: sha256(&[&btc_txid[..], b"/first-input"].concat()),
        vout: 1,
    }
}

pub fn sponsored(u: &User, nonce: u64, fee: u128, max_gas: u64, action: Action) -> Entry {
    Entry::Sponsored {
        op: op(u, nonce, FeeMode::Token { amount: fee }, max_gas, action),
    }
}

pub fn forced(u: &User, nonce: u64, max_gas: u64, action: Action) -> Entry {
    let btc_txid = sha256(&nonce.to_le_bytes());
    Entry::Forced {
        btc_txid,
        op: native_op(u, nonce, btc_txid, max_gas, action),
    }
}

/// Unlock up to acceptance position `(height, index)` (D-079).
pub fn unlock(height: u64, index: u32) -> Entry {
    Entry::Unlock { height, index }
}

/// The `op_id` of a VmOp entry.
pub fn op_id_of(entry: &Entry) -> [u8; 32] {
    match entry {
        Entry::Forced { op, .. } | Entry::Sponsored { op } => op.op_id().unwrap(),
        other => panic!("not a VmOp entry: {other:?}"),
    }
}

/// Test BTC view: the blocks in the settled closure come from a set; when `fail` is true every query returns a data
/// source failure.
#[derive(Clone, Debug, Default)]
pub struct TestBtc {
    /// Blocks that have entered the settled closure.
    pub settled: BTreeSet<[u8; 32]>,
    /// Carrier of each block (D-095): a block not registered here has no carrier.
    pub carriers: BTreeMap<[u8; 32], [u8; 32]>,
    /// Outputs of native transactions (`native_tx`, D-078): transaction ID → (amount, locking script).
    pub native: BTreeMap<[u8; 32], Vec<(u64, Vec<u8>)>>,
    /// First input of native transactions (D-086): a transaction not registered here is answered by
    /// [`test_first_input`].
    pub first_inputs: BTreeMap<[u8; 32], OutPoint>,
    /// Transaction IDs the anchor chain has no native transaction for (D-100); every other ID names a native
    /// transaction carrying whatever op the entry has.
    pub absent: BTreeSet<[u8; 32]>,
    pub fail: bool,
}

impl TestBtc {
    /// Block `block_hash` has entered the settled closure (its data has been carried by a transaction confirmed on
    /// BTC).
    pub fn settle(&mut self, block_hash: [u8; 32]) {
        self.settled.insert(block_hash);
    }

    /// Block `block_hash` has entered the settled closure and its carrier is `carrier` (D-095).
    pub fn settle_by(&mut self, block_hash: [u8; 32], carrier: [u8; 32]) {
        self.settled.insert(block_hash);
        self.carriers.insert(block_hash, carrier);
    }

    fn check(&self) -> Result<(), String> {
        if self.fail {
            Err("simulated BTC data source failure".into())
        } else {
            Ok(())
        }
    }
}

impl BtcView for TestBtc {
    fn header(&self, _height: u32) -> Result<Option<[u8; 80]>, String> {
        self.check()?;
        Ok(None)
    }

    fn block_settled(&self, block_hash: &[u8; 32]) -> Result<bool, String> {
        self.check()?;
        Ok(self.settled.contains(block_hash))
    }

    fn carrier(&self, block_hash: &[u8; 32]) -> Result<Option<[u8; 32]>, String> {
        self.check()?;
        Ok(self.carriers.get(block_hash).copied())
    }

    fn native_input(&self, txid: &[u8; 32], _op: &VmOp) -> Result<Option<OutPoint>, String> {
        self.check()?;
        if self.absent.contains(txid) {
            return Ok(None);
        }
        Ok(Some(
            self.first_inputs
                .get(txid)
                .cloned()
                .unwrap_or_else(|| test_first_input(txid)),
        ))
    }

    fn native_outputs(&self, txid: &[u8; 32]) -> Result<Vec<(u64, Vec<u8>)>, String> {
        self.check()?;
        self.native
            .get(txid)
            .cloned()
            .ok_or_else(|| "the test view has no such native transaction".into())
    }
}

pub fn transfer(to: [u8; 32], amount: u128) -> Action {
    Action::AssetTransfer {
        asset: KSA,
        to,
        amount,
    }
}

/// A small chain executing block by block on an in-memory state. Block `h` has hash `sha256(h as u64 little-endian)`
/// and DAA score `h`; the BTC view during execution is `btc` (by default without any BTC data, same as `NoBtc`).
///
/// The chain records the blocks accepted at each height (`main`) and gives the unlockable segment by the same rules as
/// the node and indexer (D-079): the positions after the parent state's unlock cursor, at most `MAX_UNLOCK_BATCH`.
pub struct Chain {
    pub exec: Executor,
    pub state: MemState,
    pub btc: TestBtc,
    pub height: u64,
    /// Hash of the latest main-chain block.
    pub parent: [u8; 32],
    pub producer: [u8; 32],
    /// Blocks accepted at each height (the index is the height; the genesis block accepts nothing): merged blocks
    /// first, the main-chain block itself last.
    pub main: Vec<Vec<UnlockItem>>,
}

/// A merged block's color in the merge set of the main-chain block merging it, its DAA score and its weight (the basis
/// of the block reward, D-074, D-084).
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub blue: bool,
    pub daa_score: u64,
    /// Number of reward slots: 1 for a non-empty block, `EMPTY_BLOCK_DIFFICULTY` for an empty block.
    pub weight: u64,
}

impl Chain {
    /// A chain without block rewards ([`NO_REWARD`]).
    pub fn new(allocations: Vec<([u8; 32], u128)>) -> Self {
        Chain::with_params(NO_REWARD, allocations)
    }

    pub fn with_params(params: Params, allocations: Vec<([u8; 32], u128)>) -> Self {
        Chain::with_genesis(params, GenesisConfig::devnet(allocations))
    }

    /// With the given genesis config (e.g. with a block share account, D-089).
    pub fn with_genesis(params: Params, genesis: GenesisConfig) -> Self {
        let mut state = MemState::default();
        state.apply(&genesis.writes().unwrap());
        Chain {
            exec: Executor::new(params).unwrap(),
            state,
            btc: TestBtc::default(),
            height: 0,
            parent: sha256(b"genesis"),
            producer: user("producer").account,
            main: vec![Vec::new()],
        }
    }

    pub fn env(&self) -> BlockEnv {
        BlockEnv {
            height: self.height + 1,
            btc_anchor: BtcAnchor {
                height: 200,
                hash: [9; 32],
            },
            producer: self.producer,
            timestamp: self.timestamp(),
            rules: self.exec.params().rules(self.height + 1),
            unlockable: self.unlockable(),
        }
    }

    /// The next block's unlockable segment (D-079): the positions after the current state's unlock cursor, at most
    /// `MAX_UNLOCK_BATCH`.
    pub fn unlockable(&self) -> Vec<UnlockItem> {
        let cursor = self.cursor();
        let max = usize::try_from(self.exec.params().max_unlock_batch).unwrap();
        self.main
            .iter()
            .flatten()
            .filter(|item| cursor.is_none_or(|c| item.position() > c))
            .take(max)
            .cloned()
            .collect()
    }

    /// The unlock cursor (the position unlocked up to).
    pub fn cursor(&self) -> Option<(u64, u32)> {
        self.get::<UnlockCursor>(&keys::unlock_cursor())
            .map(|c| (c.height, c.index))
    }

    /// Acceptance position of a block.
    pub fn position(&self, hash: &[u8; 32]) -> (u64, u32) {
        self.main
            .iter()
            .flatten()
            .find(|item| item.hash == *hash)
            .map(UnlockItem::position)
            .unwrap_or_else(|| panic!("block {} is not on the chain", hex::encode(hash)))
    }

    /// An entry unlocking up to the acceptance position of block `hash`.
    pub fn unlock_to(&self, hash: &[u8; 32]) -> Entry {
        let (height, index) = self.position(hash);
        unlock(height, index)
    }

    /// Timestamp of the next block: starting at 1 700 000 000, plus 10 seconds per block.
    pub fn timestamp(&self) -> u64 {
        1_700_000_000 + 10 * (self.height + 1)
    }

    /// Executes one block; on success applies the writes and advances.
    pub fn block(&mut self, entries: Vec<Entry>) -> Result<BlockOutcome, BlockError> {
        self.chain_block(Vec::new(), entries)
    }

    /// Executes a chain block merging `merged` (D-063); on success applies the writes and advances. All merged blocks
    /// are treated as blue with the same DAA score as this block.
    pub fn chain_block(
        &mut self,
        merged: Vec<MergedBlock>,
        entries: Vec<Entry>,
    ) -> Result<BlockOutcome, BlockError> {
        let daa_score = self.height + 1;
        let colored = merged
            .into_iter()
            .map(|m| {
                (
                    m,
                    Color {
                        blue: true,
                        daa_score,
                        weight: 1,
                    },
                )
            })
            .collect();
        self.colored_block(colored, entries)
    }

    /// Executes a chain block merging `merged` (each with its color and DAA score); on success applies the writes,
    /// records each block's acceptance position and advances.
    pub fn colored_block(
        &mut self,
        merged: Vec<(MergedBlock, Color)>,
        entries: Vec<Entry>,
    ) -> Result<BlockOutcome, BlockError> {
        let env = self.env();
        let blocks: Vec<MergedBlock> = merged.iter().map(|(m, _)| m.clone()).collect();
        let out = self.exec.execute_chain_block(
            Arc::new(self.state.clone()),
            Arc::new(self.btc.clone()),
            &env,
            &blocks,
            &entries,
        )?;
        self.state.apply(&out.writes);
        self.height = env.height;
        self.parent = sha256(&self.height.to_le_bytes());
        let mut accepted: Vec<UnlockItem> = merged
            .iter()
            .enumerate()
            .map(|(i, (m, color))| UnlockItem {
                height: self.height,
                index: u32::try_from(i).unwrap(),
                hash: m.hash,
                producer: m.producer,
                daa_score: color.daa_score,
                weight: color.weight,
                rewarded: color.blue,
            })
            .collect();
        accepted.push(UnlockItem {
            height: self.height,
            index: u32::try_from(merged.len()).unwrap(),
            hash: self.parent,
            producer: self.producer,
            daa_score: self.height,
            weight: 1,
            rewarded: true,
        });
        self.main.push(accepted);
        Ok(out)
    }

    pub fn get<T: borsh::BorshDeserialize>(&self, key: &[u8]) -> Option<T> {
        self.state.0.get(key).map(|v| borsh::from_slice(v).unwrap())
    }

    pub fn ksa(&self, account: &[u8; 32]) -> u128 {
        self.get::<u128>(&ksana_execution::keys::balance(&KSA, account))
            .unwrap_or(0)
    }

    pub fn nonce(&self, account: &[u8; 32]) -> u64 {
        self.account(account).nonce
    }

    pub fn account(&self, account: &[u8; 32]) -> AccountState {
        self.get(&ksana_execution::keys::account(account))
            .unwrap_or_default()
    }

    /// The not yet unlocked sponsored-fee record of block `block_hash` (by its acceptance position).
    pub fn locked_fee(&self, block_hash: &[u8; 32]) -> Option<LockedFee> {
        let (height, index) = self.position(block_hash);
        self.get(&keys::locked_fee(height, index))
    }
}

/// Minimal contract template (WAT): memory, allocator, Ok wrapper.
pub const RUNTIME: &str = r#"
  (import "ksana" "storage_set" (func $storage_set (param i32 i32 i32 i32)))
  (memory (export "memory") 2)
  (global $heap (mut i32) (i32.const 65536))
  (func $alloc (export "ksana_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $p))
  (func $ok (result i64)
    (local $out i32)
    (local.set $out (call $alloc (i32.const 5)))
    (i32.store8 (local.get $out) (i32.const 1))
    (i32.store (i32.add (local.get $out) (i32.const 1)) (i32.const 0))
    (i64.or (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32)) (i64.const 5)))
  (data (i32.const 100) "key")
  (data (i32.const 110) "val")
"#;

pub fn wat_module(body: &str) -> Vec<u8> {
    wat::parse_str(format!("(module {RUNTIME} {body})")).unwrap()
}

/// Writes to storage, then returns successfully.
pub fn writer_contract() -> Vec<u8> {
    wat_module(
        r#"(func (export "ksana_call") (param i32 i32) (result i64)
             (call $storage_set (i32.const 100) (i32.const 3) (i32.const 110) (i32.const 3))
             (call $ok))"#,
    )
}

/// init succeeds; other methods write to storage and then trap.
pub fn trap_after_write_contract() -> Vec<u8> {
    wat_module(
        r#"(func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
             ;; succeed immediately when the method name length is 4 ("init")
             (if (i32.eq (i32.load (local.get $p)) (i32.const 4)) (then (return (call $ok))))
             (call $storage_set (i32.const 100) (i32.const 3) (i32.const 110) (i32.const 3))
             unreachable)"#,
    )
}

/// init succeeds; other methods write to storage and then loop forever (running out of gas).
pub fn spin_after_write_contract() -> Vec<u8> {
    wat_module(
        r#"(func (export "ksana_call") (param $p i32) (param $l i32) (result i64)
             (if (i32.eq (i32.load (local.get $p)) (i32.const 4)) (then (return (call $ok))))
             (call $storage_set (i32.const 100) (i32.const 3) (i32.const 110) (i32.const 3))
             (loop $l (br $l))
             (call $ok))"#,
    )
}

/// A contract containing floating-point instructions.
pub fn float_contract() -> Vec<u8> {
    wat_module(
        r#"(func (export "ksana_call") (param i32 i32) (result i64)
             (drop (f64.add (f64.const 1) (f64.const 2)))
             (call $ok))"#,
    )
}

pub fn deploy(code: Vec<u8>) -> Action {
    Action::Deploy {
        code,
        init_args: vec![],
        funds: Vec::new(),
    }
}
