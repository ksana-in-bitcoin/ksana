//! Genesis state (SPEC §8.6: devnet pre-allocates KSA to test accounts in the genesis block; block rewards see D-074).

use ksana_primitives::{Params, KSA_ASSET_ID};

use crate::executor::BlockError;
use crate::keys::{self, AssetMeta};
use crate::state::WriteSet;

/// Genesis config (configurable parameters; issuance and allocation of KSA see SPEC §8.6, D-089).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenesisConfig {
    /// Name of KSA.
    pub ksa_name: String,
    /// Symbol of KSA (D-121).
    pub ksa_symbol: String,
    /// Decimals of KSA.
    pub ksa_decimals: u8,
    /// Pre-allocations: account and amount.
    pub allocations: Vec<([u8; 32], u128)>,
    /// Block share account (D-089): the `BLOCK_SHARE_BPS` share of every reward slot is credited to it; with `None`
    /// the whole block reward goes to the producer.
    pub block_share_account: Option<[u8; 32]>,
}

impl GenesisConfig {
    /// devnet default: KSA (name and symbol), 8 decimals, the given pre-allocations.
    pub fn devnet(allocations: Vec<([u8; 32], u128)>) -> Self {
        GenesisConfig {
            ksa_name: "KSA".into(),
            ksa_symbol: "KSA".into(),
            ksa_decimals: 8,
            allocations,
            block_share_account: None,
        }
    }

    /// Sets the block share account (D-089).
    pub fn with_block_share_account(mut self, account: Option<[u8; 32]>) -> Self {
        self.block_share_account = account;
        self
    }

    /// The total pre-allocation does not exceed `KSA_MAX_SUPPLY − upper bound of total rewards` (D-074).
    pub fn check_supply(&self, params: &Params) -> Result<(), BlockError> {
        let mut total: u128 = 0;
        for (_, amount) in &self.allocations {
            total = total
                .checked_add(*amount)
                .ok_or_else(|| BlockError::Invalid("genesis allocation total overflows".into()))?;
        }
        if total > params.max_genesis_supply() {
            return Err(BlockError::Invalid(format!(
                "genesis allocation total {total} exceeds the limit {} (KSA_MAX_SUPPLY minus the upper bound of total block rewards)",
                params.max_genesis_supply()
            )));
        }
        Ok(())
    }

    /// The write set of the genesis state (version 0). Errors when the same account is allocated twice or the total
    /// overflows.
    pub fn writes(&self) -> Result<WriteSet, BlockError> {
        let mut writes = WriteSet::new();
        let mut total: u128 = 0;
        for (account, amount) in &self.allocations {
            let key = keys::balance(&KSA_ASSET_ID, account);
            if writes.contains_key(&key) {
                return Err(BlockError::Invalid(
                    "duplicate account in the genesis allocations".into(),
                ));
            }
            total = total
                .checked_add(*amount)
                .ok_or_else(|| BlockError::Invalid("genesis allocation total overflows".into()))?;
            if *amount > 0 {
                writes.insert(key, Some(encode(amount)?));
            }
        }
        // KSA has no issuing contract and no capabilities (D-121)
        let meta = AssetMeta {
            name: self.ksa_name.clone(),
            symbol: self.ksa_symbol.clone(),
            decimals: self.ksa_decimals,
            total_supply: total,
            issuer: [0u8; 32],
            capabilities: 0,
            paused: false,
        };
        writes.insert(keys::asset(&KSA_ASSET_ID), Some(encode(&meta)?));
        if let Some(account) = &self.block_share_account {
            writes.insert(keys::block_share_account(), Some(encode(account)?));
        }
        Ok(writes)
    }
}

fn encode<T: borsh::BorshSerialize>(v: &T) -> Result<Vec<u8>, BlockError> {
    borsh::to_vec(v).map_err(|e| BlockError::Fatal(e.to_string()))
}
