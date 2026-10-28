//! Execution-layer test utilities using the test contract `probe` (`contracts/testing/probe`): a small chain where
//! every op is its own block.

use std::collections::BTreeMap;

use ksana_execution::builtin::contract_id;
use ksana_execution::{OpStatus, Receipt};
use ksana_primitives::hash::sha256;
use ksana_primitives::{Action, Funds};
use ksana_test_contracts::PROBE;

use super::{sponsored, Chain, User, KSA};

pub const GAS: u64 = 20_000_000;
pub const FEE: u128 = 7;
pub const START: u128 = 1_000_000;

pub fn ksa(amount: u128) -> Funds {
    Funds { asset: KSA, amount }
}

pub fn enc<T: borsh::BorshSerialize>(v: &T) -> Vec<u8> {
    borsh::to_vec(v).unwrap()
}

pub fn deploy_probe(tag: &[u8], funds: Vec<Funds>) -> Action {
    Action::Deploy {
        code: PROBE.to_vec(),
        init_args: enc(&tag.to_vec()),
        funds,
    }
}

pub fn call(contract: [u8; 32], method: &str, args: Vec<u8>, funds: Vec<Funds>) -> Action {
    Action::Call {
        contract,
        method: method.into(),
        args,
        funds,
    }
}

/// Arguments of `probe.forward` / `forward_then_revert`.
pub fn forward(target: [u8; 32], method: &str, inner: Vec<u8>, funds: Vec<Funds>) -> Vec<u8> {
    enc(&(target, method.to_string(), inner, funds))
}

pub fn failed(r: &Receipt) -> &str {
    match &r.status {
        OpStatus::Failed(reason) => reason,
        other => panic!("should have failed: {other:?}"),
    }
}

/// A small chain executing every op as its own block, tracking each account's nonce.
pub struct World {
    pub chain: Chain,
    pub nonces: BTreeMap<[u8; 32], u64>,
}

impl World {
    pub fn new(users: &[&User]) -> Self {
        World {
            chain: Chain::new(users.iter().map(|u| (u.account, START)).collect()),
            nonces: BTreeMap::new(),
        }
    }

    pub fn run(&mut self, u: &User, action: Action) -> Receipt {
        self.run_gas(u, GAS, action)
    }

    pub fn run_gas(&mut self, u: &User, max_gas: u64, action: Action) -> Receipt {
        let nonce = self.nonces.entry(u.account).or_insert(0);
        let entry = sponsored(u, *nonce, FEE, max_gas, action);
        *nonce += 1;
        let mut out = self
            .chain
            .block(vec![entry])
            .expect("block should be valid");
        out.receipts.remove(0)
    }

    /// Deploys probe and returns the contract ID.
    pub fn deploy(&mut self, u: &User, funds: Vec<Funds>) -> [u8; 32] {
        let nonce = self.nonces.get(&u.account).copied().unwrap_or(0);
        let r = self.run(u, deploy_probe(b"ok", funds));
        assert_eq!(r.status, OpStatus::Success, "{r:?}");
        contract_id(&u.account, nonce, &sha256(PROBE))
    }

    pub fn balance(&self, asset: &[u8; 32], account: &[u8; 32]) -> u128 {
        self.chain
            .get::<u128>(&ksana_execution::keys::balance(asset, account))
            .unwrap_or(0)
    }
}
