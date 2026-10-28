//! State keys and values (SPEC §10.1; encoding see docs/ABI.md section 7, DECISIONS D-019, D-079; 0x07 was the
//! funds record of path A, no longer used since D-078; 0x0A was an element of the pending-unlock queue, no longer used
//! since D-079; 0x0B is the block share account, D-089; 0x0C is an account's status for an asset, D-121).

use borsh::{BorshDeserialize, BorshSerialize};

/// 0x01 account: nonce.
pub const PREFIX_ACCOUNT: u8 = 0x01;
/// 0x02 asset balance.
pub const PREFIX_BALANCE: u8 = 0x02;
/// 0x03 asset metadata.
pub const PREFIX_ASSET: u8 = 0x03;
/// 0x04 contract metadata.
pub const PREFIX_CONTRACT: u8 = 0x04;
/// 0x05 contract storage.
pub const PREFIX_STORAGE: u8 = 0x05;
/// 0x06 contract code.
pub const PREFIX_CODE: u8 = 0x06;
/// 0x08 sponsored fees pending unlock (by acceptance position, D-079).
pub const PREFIX_LOCKED_FEE: u8 = 0x08;
/// 0x09 unlock cursor: the acceptance position unlocked up to (D-079).
pub const PREFIX_UNLOCK_CURSOR: u8 = 0x09;
/// 0x0B block share account (D-089): the account given by the genesis config that receives the `BLOCK_SHARE_BPS` share
/// of every reward slot.
pub const PREFIX_BLOCK_SHARE_ACCOUNT: u8 = 0x0B;
/// 0x0C an account's status flags for an asset (D-121): frozen, allowed.
pub const PREFIX_ASSET_STATUS: u8 = 0x0C;

fn key(prefix: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len = parts
        .iter()
        .fold(1usize, |acc, p| acc.saturating_add(p.len()));
    let mut k = Vec::with_capacity(len);
    k.push(prefix);
    for p in parts {
        k.extend_from_slice(p);
    }
    k
}

/// `0x01 ‖ account`
pub fn account(account: &[u8; 32]) -> Vec<u8> {
    key(PREFIX_ACCOUNT, &[account])
}

/// `0x02 ‖ asset_id ‖ account`
pub fn balance(asset: &[u8; 32], account: &[u8; 32]) -> Vec<u8> {
    key(PREFIX_BALANCE, &[asset, account])
}

/// `0x03 ‖ asset_id`
pub fn asset(asset: &[u8; 32]) -> Vec<u8> {
    key(PREFIX_ASSET, &[asset])
}

/// `0x04 ‖ contract_id`
pub fn contract(contract: &[u8; 32]) -> Vec<u8> {
    key(PREFIX_CONTRACT, &[contract])
}

/// `0x05 ‖ contract_id ‖ key`
pub fn storage(contract: &[u8; 32], k: &[u8]) -> Vec<u8> {
    key(PREFIX_STORAGE, &[contract, k])
}

/// `0x06 ‖ code_hash`
pub fn code(code_hash: &[u8; 32]) -> Vec<u8> {
    key(PREFIX_CODE, &[code_hash])
}

/// `0x08 ‖ height (u64 big-endian) ‖ index (u32 big-endian)` (D-079): the sponsored fee of the block at acceptance
/// position `(height, index)`.
pub fn locked_fee(height: u64, index: u32) -> Vec<u8> {
    key(
        PREFIX_LOCKED_FEE,
        &[&height.to_be_bytes(), &index.to_be_bytes()],
    )
}

/// Recovers the acceptance position from a [`locked_fee`] key; `None` when it is not in that format.
pub fn locked_fee_position(key: &[u8]) -> Option<(u64, u32)> {
    let (prefix, rest) = key.split_first()?;
    if *prefix != PREFIX_LOCKED_FEE || rest.len() != 12 {
        return None;
    }
    let height = u64::from_be_bytes(rest.get(..8)?.try_into().ok()?);
    let index = u32::from_be_bytes(rest.get(8..)?.try_into().ok()?);
    Some((height, index))
}

/// `0x09` (D-079)
pub fn unlock_cursor() -> Vec<u8> {
    key(PREFIX_UNLOCK_CURSOR, &[])
}

/// `0x0B` (D-089): the value is the block share account (`[u8; 32]`, written by the genesis config); when absent the
/// whole block reward goes to the producer.
pub fn block_share_account() -> Vec<u8> {
    key(PREFIX_BLOCK_SHARE_ACCOUNT, &[])
}

/// `0x0C ‖ asset_id ‖ account` (D-121): the value is the account's status flags for the asset (`u8`,
/// `ksana_primitives::asset::STATUS_*`); absent when it has none.
pub fn asset_status(asset: &[u8; 32], account: &[u8; 32]) -> Vec<u8> {
    key(PREFIX_ASSET_STATUS, &[asset, account])
}

/// Value of 0x01.
#[derive(Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AccountState {
    /// The next usable nonce.
    pub nonce: u64,
}

/// Value of 0x03.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AssetMeta {
    /// Name (1 to 64 bytes, D-121).
    pub name: String,
    /// Symbol (1 to 16 bytes, D-121).
    pub symbol: String,
    /// Decimals (at most 18, D-121).
    pub decimals: u8,
    /// Total supply.
    pub total_supply: u128,
    /// Issuing contract (32 zero bytes for KSA).
    pub issuer: [u8; 32],
    /// The capabilities the issuing contract still holds (`ksana_primitives::asset::CAP_*`, D-121): declared when the
    /// asset is created, afterwards only renounced. KSA has none.
    pub capabilities: u8,
    /// Whether the issuing contract paused the asset (only with `CAP_PAUSE`, D-121).
    pub paused: bool,
}

/// Value of 0x04.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ContractMeta {
    /// `sha256(code)`.
    pub code_hash: [u8; 32],
    /// Deployer account.
    pub deployer: [u8; 32],
}

/// Value of 0x08: a block's path-B sponsored fee not yet unlocked (exists only when that block collected sponsored
/// fees, deleted at unlock, D-079). Block rewards are not recorded; they are paid out directly by segment at unlock
/// time.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct LockedFee {
    /// Producer (`header.producer`).
    pub producer: [u8; 32],
    /// The locked KSA amount.
    pub amount: u128,
}

/// Value of 0x09 (D-079): the acceptance position unlocked up to (inclusive). When the key is absent nothing has been
/// unlocked yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, BorshSerialize, BorshDeserialize)]
pub struct UnlockCursor {
    /// Height of the main-chain block that accepted it.
    pub height: u64,
    /// Index in that main-chain block's acceptance order.
    pub index: u32,
}
