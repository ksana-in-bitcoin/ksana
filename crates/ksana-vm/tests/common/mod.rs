//! VM test utilities: an in-memory `Host` and WAT module templates.

#![allow(dead_code)]

use std::collections::BTreeMap;

use ksana_primitives::hash::sha256;
use ksana_vm::{AssetMetaInput, AssetPolicy, ContractCode, Host, HostError, NativeTxInfo};

/// The state snapshot at a checkpoint: storage, balances, assets, event count.
pub struct Snapshot {
    storage: BTreeMap<([u8; 32], Vec<u8>), Vec<u8>>,
    balances: BTreeMap<([u8; 32], [u8; 32]), u128>,
    assets: BTreeMap<[u8; 32], (AssetMetaInput, u128, [u8; 32])>,
    policies: BTreeMap<[u8; 32], AssetPolicy>,
    statuses: BTreeMap<([u8; 32], [u8; 32]), u8>,
    contracts: BTreeMap<[u8; 32], ContractCode>,
    creators: BTreeMap<[u8; 32], [u8; 32]>,
    events: usize,
}

/// An in-memory host: contract storage, assets, contract code, BTC block headers, events.
#[derive(Default)]
pub struct MockHost {
    pub checkpoints: Vec<Snapshot>,
    pub storage: BTreeMap<([u8; 32], Vec<u8>), Vec<u8>>,
    pub balances: BTreeMap<([u8; 32], [u8; 32]), u128>,
    pub assets: BTreeMap<[u8; 32], (AssetMetaInput, u128, [u8; 32])>,
    /// The capabilities and pause state of the assets (D-121; absent for an asset first issued by `asset_issue`).
    pub policies: BTreeMap<[u8; 32], AssetPolicy>,
    /// The status flags of accounts for assets (D-121).
    pub statuses: BTreeMap<([u8; 32], [u8; 32]), u8>,
    /// Every call of the asset management functions, in order (D-121).
    pub admin: Vec<String>,
    pub contracts: BTreeMap<[u8; 32], ContractCode>,
    pub headers: BTreeMap<u32, [u8; 80]>,
    pub events: Vec<([u8; 32], Vec<u8>)>,
    pub height: u64,
    pub timestamp: u64,
    pub anchor: u32,
    /// Code already on chain (by code hash).
    pub codes: BTreeMap<[u8; 32], Vec<u8>>,
    /// The creators of contracts (`create_contract`).
    pub creators: BTreeMap<[u8; 32], [u8; 32]>,
    /// The next storage_get returns a fatal error.
    pub fail_fatal: bool,
    /// The BTC transaction carrying the current op (`native_tx`, D-078); `None` simulates a path-B op.
    pub native: Option<NativeTxInfo>,
}

impl MockHost {
    pub fn deploy(&mut self, id: [u8; 32], code: Vec<u8>) {
        self.codes.insert(sha256(&code), code.clone());
        self.contracts.insert(
            id,
            ContractCode {
                code_hash: sha256(&code),
                code,
            },
        );
    }
}

impl Host for MockHost {
    fn storage_get(
        &mut self,
        contract: &[u8; 32],
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, HostError> {
        if self.fail_fatal {
            return Err(HostError::Fatal("disk error".into()));
        }
        Ok(self.storage.get(&(*contract, key.to_vec())).cloned())
    }
    fn storage_set(
        &mut self,
        contract: &[u8; 32],
        key: &[u8],
        value: &[u8],
    ) -> Result<(), HostError> {
        self.storage
            .insert((*contract, key.to_vec()), value.to_vec());
        Ok(())
    }
    fn storage_remove(&mut self, contract: &[u8; 32], key: &[u8]) -> Result<(), HostError> {
        self.storage.remove(&(*contract, key.to_vec()));
        Ok(())
    }
    fn block_height(&self) -> u64 {
        self.height
    }
    fn block_timestamp(&self) -> u64 {
        self.timestamp
    }
    fn asset_burn(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        let balance = self.balances.get(&(*asset, *from)).copied().unwrap_or(0);
        let rest = balance
            .checked_sub(amount)
            .ok_or_else(|| HostError::Failed("insufficient".into()))?;
        let entry = self
            .assets
            .get_mut(asset)
            .ok_or_else(|| HostError::Failed("unknown asset".into()))?;
        entry.1 = entry
            .1
            .checked_sub(amount)
            .ok_or_else(|| HostError::Fatal("supply below balance".into()))?;
        self.balances.insert((*asset, *from), rest);
        Ok(())
    }
    fn code(&mut self, code_hash: &[u8; 32]) -> Result<Option<Vec<u8>>, HostError> {
        Ok(self.codes.get(code_hash).cloned())
    }
    fn create_contract(
        &mut self,
        contract: &[u8; 32],
        code_hash: &[u8; 32],
        creator: &[u8; 32],
    ) -> Result<(), HostError> {
        let code = self
            .codes
            .get(code_hash)
            .cloned()
            .ok_or_else(|| HostError::Fatal("no code".into()))?;
        self.contracts.insert(
            *contract,
            ContractCode {
                code_hash: *code_hash,
                code,
            },
        );
        self.creators.insert(*contract, *creator);
        Ok(())
    }
    fn btc_anchor_height(&self) -> u32 {
        self.anchor
    }
    fn btc_header(&mut self, height: u32) -> Result<Option<[u8; 80]>, HostError> {
        Ok(self.headers.get(&height).copied())
    }
    fn native_tx(&mut self) -> Result<Option<NativeTxInfo>, HostError> {
        Ok(self.native.clone())
    }
    fn asset_balance(&mut self, asset: &[u8; 32], account: &[u8; 32]) -> Result<u128, HostError> {
        Ok(self.balances.get(&(*asset, *account)).copied().unwrap_or(0))
    }
    fn asset_transfer(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        let from_bal = self.balances.get(&(*asset, *from)).copied().unwrap_or(0);
        let rest = from_bal
            .checked_sub(amount)
            .ok_or_else(|| HostError::Failed("insufficient".into()))?;
        self.balances.insert((*asset, *from), rest);
        *self.balances.entry((*asset, *to)).or_insert(0) += amount;
        Ok(())
    }
    fn asset_issue(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        meta: &AssetMetaInput,
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        let entry = self
            .assets
            .entry(*asset)
            .or_insert_with(|| (meta.clone(), 0, *issuer));
        if entry.0 != *meta {
            return Err(HostError::Failed("meta mismatch".into()));
        }
        entry.1 = entry
            .1
            .checked_add(amount)
            .ok_or_else(|| HostError::Failed("supply overflow".into()))?;
        *self.balances.entry((*asset, *to)).or_insert(0) += amount;
        Ok(())
    }
    fn asset_create(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        meta: &AssetMetaInput,
        capabilities: u8,
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        if self.assets.contains_key(asset) {
            return Err(HostError::Failed("asset already exists".into()));
        }
        self.assets.insert(*asset, (meta.clone(), amount, *issuer));
        self.policies.insert(
            *asset,
            AssetPolicy {
                capabilities,
                paused: false,
            },
        );
        *self.balances.entry((*asset, *to)).or_insert(0) += amount;
        self.admin.push(format!("create {capabilities}"));
        Ok(())
    }
    fn asset_set_status(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        account: &[u8; 32],
        flag: u8,
        on: bool,
    ) -> Result<(), HostError> {
        match self.assets.get(asset) {
            Some((_, _, i)) if i == issuer => {}
            _ => return Err(HostError::Failed("asset does not exist".into())),
        }
        let status = self.statuses.entry((*asset, *account)).or_insert(0);
        *status = if on { *status | flag } else { *status & !flag };
        self.admin.push(format!("status {flag} {on}"));
        Ok(())
    }
    fn asset_pause(
        &mut self,
        asset: &[u8; 32],
        _issuer: &[u8; 32],
        paused: bool,
    ) -> Result<(), HostError> {
        let policy = self
            .policies
            .get_mut(asset)
            .ok_or_else(|| HostError::Failed("asset does not exist".into()))?;
        policy.paused = paused;
        self.admin.push(format!("pause {paused}"));
        Ok(())
    }
    fn asset_renounce(
        &mut self,
        asset: &[u8; 32],
        _issuer: &[u8; 32],
        capabilities: u8,
    ) -> Result<(), HostError> {
        let policy = self
            .policies
            .get_mut(asset)
            .ok_or_else(|| HostError::Failed("asset does not exist".into()))?;
        policy.capabilities &= !capabilities;
        self.admin.push(format!("renounce {capabilities}"));
        Ok(())
    }
    fn asset_policy(&mut self, asset: &[u8; 32]) -> Result<Option<AssetPolicy>, HostError> {
        Ok(match self.policies.get(asset) {
            Some(p) => Some(*p),
            None if self.assets.contains_key(asset) => Some(AssetPolicy {
                capabilities: 1,
                paused: false,
            }),
            None => None,
        })
    }
    fn asset_status(&mut self, asset: &[u8; 32], account: &[u8; 32]) -> Result<u8, HostError> {
        Ok(self.statuses.get(&(*asset, *account)).copied().unwrap_or(0))
    }
    fn contract_code(&mut self, contract: &[u8; 32]) -> Result<Option<ContractCode>, HostError> {
        Ok(self.contracts.get(contract).cloned())
    }
    fn emit_event(&mut self, contract: &[u8; 32], data: &[u8]) -> Result<(), HostError> {
        self.events.push((*contract, data.to_vec()));
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), HostError> {
        self.checkpoints.push(Snapshot {
            storage: self.storage.clone(),
            balances: self.balances.clone(),
            assets: self.assets.clone(),
            policies: self.policies.clone(),
            statuses: self.statuses.clone(),
            contracts: self.contracts.clone(),
            creators: self.creators.clone(),
            events: self.events.len(),
        });
        Ok(())
    }
    fn revert_checkpoint(&mut self) -> Result<(), HostError> {
        let snapshot = self
            .checkpoints
            .pop()
            .ok_or_else(|| HostError::Fatal("no checkpoint".into()))?;
        self.storage = snapshot.storage;
        self.balances = snapshot.balances;
        self.assets = snapshot.assets;
        self.policies = snapshot.policies;
        self.statuses = snapshot.statuses;
        self.contracts = snapshot.contracts;
        self.creators = snapshot.creators;
        self.events.truncate(snapshot.events);
        Ok(())
    }
    fn release_checkpoint(&mut self) -> Result<(), HostError> {
        self.checkpoints
            .pop()
            .map(|_| ())
            .ok_or_else(|| HostError::Fatal("no checkpoint".into()))
    }
}

/// The import declarations of all host functions (WAT).
pub const IMPORTS: &str = r#"
  (import "ksana" "storage_get" (func $storage_get (param i32 i32) (result i64)))
  (import "ksana" "storage_set" (func $storage_set (param i32 i32 i32 i32)))
  (import "ksana" "storage_remove" (func $storage_remove (param i32 i32)))
  (import "ksana" "caller" (func $caller (param i32)))
  (import "ksana" "self_id" (func $self_id (param i32)))
  (import "ksana" "block_height" (func $block_height (result i64)))
  (import "ksana" "block_timestamp" (func $block_timestamp (result i64)))
  (import "ksana" "chain_id" (func $chain_id (result i64)))
  (import "ksana" "gas_left" (func $gas_left (result i64)))
  (import "ksana" "btc_anchor_height" (func $btc_anchor_height (result i64)))
  (import "ksana" "btc_header" (func $btc_header (param i64 i32)))
  (import "ksana" "asset_balance" (func $asset_balance (param i32 i32 i32)))
  (import "ksana" "asset_transfer" (func $asset_transfer (param i32 i32 i32)))
  (import "ksana" "asset_issue" (func $asset_issue (param i32 i32 i32 i32 i32 i32)))
  (import "ksana" "asset_burn" (func $asset_burn (param i32 i32)))
  (import "ksana" "asset_create" (func $asset_create (param i32 i32 i32 i32 i32 i32 i32)))
  (import "ksana" "asset_freeze" (func $asset_freeze (param i32 i32 i32)))
  (import "ksana" "asset_allow" (func $asset_allow (param i32 i32 i32)))
  (import "ksana" "asset_pause" (func $asset_pause (param i32 i32)))
  (import "ksana" "asset_renounce" (func $asset_renounce (param i32 i32)))
  (import "ksana" "asset_policy" (func $asset_policy (param i32) (result i64)))
  (import "ksana" "asset_status" (func $asset_status (param i32 i32) (result i32)))
  (import "ksana" "create_contract" (func $create_contract (param i32 i32 i32 i32 i32 i32 i32)))
  (import "ksana" "call_contract" (func $call_contract (param i32 i32 i32 i32 i32) (result i64)))
  (import "ksana" "call_funds" (func $call_funds (result i64)))
  (import "ksana" "native_tx" (func $native_tx (result i64)))
  (import "ksana" "try_call_contract" (func $try_call_contract (param i32 i32 i32 i32 i32 i64) (result i64)))
  (import "ksana" "delegate_call" (func $delegate_call (param i32 i32 i32) (result i64)))
  (import "ksana" "emit_event" (func $emit_event (param i32 i32)))
  (import "ksana" "sha256" (func $sha256 (param i32 i32 i32)))
  (import "ksana" "verify_schnorr" (func $verify_schnorr (param i32 i32 i32) (result i32)))
"#;

/// Memory, the heap pointer and `ksana_alloc` (linear allocation starting at 64 KiB), plus the helper functions that
/// write a `len`-byte `Ok(data)` result at `$ok_at`.
pub const RUNTIME: &str = r#"
  (memory (export "memory") 2)
  (global $heap (mut i32) (i32.const 65536))
  (func $alloc (export "ksana_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (if (i32.gt_u (global.get $heap) (i32.mul (memory.size) (i32.const 65536)))
      (then (drop (memory.grow (i32.const 1)))))
    (local.get $p))
  ;; wraps [data, data+len) as Borsh Ok(Vec<u8>) (Borsh's Result tag: 0 = Err, 1 = Ok)
  ;; into a newly allocated buffer, returning the packed (ptr<<32)|len
  (func $ok (param $data i32) (param $len i32) (result i64)
    (local $out i32)
    (local.set $out (call $alloc (i32.add (local.get $len) (i32.const 5))))
    (i32.store8 (local.get $out) (i32.const 1))
    (i32.store (i32.add (local.get $out) (i32.const 1)) (local.get $len))
    (memory.copy (i32.add (local.get $out) (i32.const 5)) (local.get $data) (local.get $len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (i32.add (local.get $len) (i32.const 5)))))
  ;; as above, but producing Borsh Err(Vec<u8>)
  (func $err (param $data i32) (param $len i32) (result i64)
    (local $out i32)
    (local.set $out (call $alloc (i32.add (local.get $len) (i32.const 5))))
    (i32.store8 (local.get $out) (i32.const 0))
    (i32.store (i32.add (local.get $out) (i32.const 1)) (local.get $len))
    (memory.copy (i32.add (local.get $out) (i32.const 5)) (local.get $data) (local.get $len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (i32.add (local.get $len) (i32.const 5)))))
  ;; the start address and length of args within the input (method: String, args: Vec<u8>)
  (func $args_ptr (param $ptr i32) (result i32)
    (i32.add (i32.add (local.get $ptr) (i32.const 8)) (i32.load (local.get $ptr))))
  (func $args_len (param $ptr i32) (result i32)
    (i32.load (i32.add (i32.add (local.get $ptr) (i32.const 4)) (i32.load (local.get $ptr)))))
"#;

/// Assembles a complete module from the imports, the runtime and a custom part, and compiles it to wasm bytes.
pub fn module(body: &str) -> Vec<u8> {
    let text = format!("(module {IMPORTS} {RUNTIME} {body})");
    wat::parse_str(&text).unwrap_or_else(|e| panic!("WAT error: {e}\n{text}"))
}

/// A raw module without the runtime and imports.
pub fn raw(text: &str) -> Vec<u8> {
    wat::parse_str(text).unwrap_or_else(|e| panic!("WAT error: {e}\n{text}"))
}

pub fn id(byte: u8) -> [u8; 32] {
    [byte; 32]
}
