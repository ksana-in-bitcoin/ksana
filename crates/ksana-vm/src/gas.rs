//! The gas table (consensus parameters, SPEC §8.3; the values are in docs/ABI.md sections 3 and 4 and DECISIONS
//! D-013).
//!
//! 1 gas = 1 wasmtime fuel; host functions additionally charge per this table before executing.

/// Fixed cost of the built-in action `AssetTransfer`.
pub const ASSET_TRANSFER_ACTION: u64 = 10_000;
/// Fixed cost of the built-in action `Deploy`.
pub const DEPLOY_BASE: u64 = 100_000;
/// Cost per byte of code for `Deploy`.
pub const DEPLOY_PER_CODE_BYTE: u64 = 20;
/// Fixed cost of a top-level `Call`.
pub const CALL_BASE: u64 = 20_000;
/// Cost per byte of loading contract code (top-level calls and `call_contract`).
pub const CALL_PER_CODE_BYTE: u64 = 1;

/// `storage_get`: fixed part.
pub const STORAGE_GET_BASE: u64 = 2_000;
/// `storage_get`: per byte (key + value).
pub const STORAGE_GET_PER_BYTE: u64 = 1;
/// `storage_set`: fixed part.
pub const STORAGE_SET_BASE: u64 = 10_000;
/// `storage_set`: per byte (key + value).
pub const STORAGE_SET_PER_BYTE: u64 = 10;
/// `storage_remove`: fixed part.
pub const STORAGE_REMOVE_BASE: u64 = 5_000;
/// `storage_remove`: per byte (key).
pub const STORAGE_REMOVE_PER_BYTE: u64 = 1;
/// `caller` / `self_id` / `block_height` / `btc_anchor_height` / `block_timestamp` / `chain_id` / `gas_left`.
pub const ENV_QUERY: u64 = 100;
/// `btc_header`.
pub const BTC_HEADER: u64 = 2_000;
/// `asset_balance`.
pub const ASSET_BALANCE: u64 = 2_000;
/// `asset_transfer`.
pub const ASSET_TRANSFER: u64 = 10_000;
/// `asset_burn` (D-073).
pub const ASSET_BURN: u64 = 10_000;
/// `create_contract`: fixed part (plus per byte of the `init` args, the callee code and the attached funds, D-073).
pub const CREATE_CONTRACT_BASE: u64 = 100_000;
/// `asset_issue`: fixed part.
pub const ASSET_ISSUE_BASE: u64 = 20_000;
/// `asset_issue`: per byte of meta.
pub const ASSET_ISSUE_PER_BYTE: u64 = 1;
/// `asset_create`: fixed part (D-121).
pub const ASSET_CREATE_BASE: u64 = 20_000;
/// `asset_create`: per byte of meta (D-121).
pub const ASSET_CREATE_PER_BYTE: u64 = 1;
/// `asset_freeze` / `asset_allow` (D-121).
pub const ASSET_SET_STATUS: u64 = 10_000;
/// `asset_pause` (D-121).
pub const ASSET_PAUSE: u64 = 10_000;
/// `asset_renounce` (D-121).
pub const ASSET_RENOUNCE: u64 = 10_000;
/// `asset_policy` (D-121).
pub const ASSET_POLICY: u64 = 2_000;
/// `asset_status` (D-121).
pub const ASSET_STATUS: u64 = 2_000;
/// `call_contract`: fixed part (plus per byte of input and of the callee code).
pub const CALL_CONTRACT_BASE: u64 = 20_000;
/// `call_contract`: per byte of input.
pub const CALL_CONTRACT_PER_INPUT_BYTE: u64 = 1;
/// Each asset item attached to a call (top-level `Call` / `Deploy` and contract-to-contract calls, D-073).
pub const FUNDS_PER_ASSET: u64 = 10_000;
/// `call_funds`: fixed part.
pub const CALL_FUNDS_BASE: u64 = 100;
/// `call_funds`: per byte of returned data.
pub const CALL_FUNDS_PER_BYTE: u64 = 1;
/// `native_tx`: fixed part (D-078).
pub const NATIVE_TX_BASE: u64 = 2_000;
/// `native_tx`: per byte of returned data.
pub const NATIVE_TX_PER_BYTE: u64 = 1;
/// `emit_event`: fixed part.
pub const EMIT_EVENT_BASE: u64 = 1_000;
/// `emit_event`: per byte.
pub const EMIT_EVENT_PER_BYTE: u64 = 5;
/// `sha256`: fixed part.
pub const SHA256_BASE: u64 = 500;
/// `sha256`: per byte.
pub const SHA256_PER_BYTE: u64 = 1;
/// `verify_schnorr`.
pub const VERIFY_SCHNORR: u64 = 50_000;

/// `base + per_byte × len`, saturating to `u64::MAX` on overflow (necessarily above any gas limit).
pub fn cost(base: u64, per_byte: u64, len: usize) -> u64 {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    base.saturating_add(per_byte.saturating_mul(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gas table is consensus (D-013): every value as docs/ABI.md sections 3 and 4 give it, so that an accidental
    /// change fails here and not only in review (the review of D-121, S5).
    #[test]
    fn the_table_holds_the_values_of_the_abi() {
        let table = [
            ("AssetTransfer", ASSET_TRANSFER_ACTION, 10_000),
            ("Deploy", DEPLOY_BASE, 100_000),
            ("Deploy per byte of code", DEPLOY_PER_CODE_BYTE, 20),
            ("Call", CALL_BASE, 20_000),
            ("loading code per byte", CALL_PER_CODE_BYTE, 1),
            ("storage_get", STORAGE_GET_BASE, 2_000),
            ("storage_get per byte", STORAGE_GET_PER_BYTE, 1),
            ("storage_set", STORAGE_SET_BASE, 10_000),
            ("storage_set per byte", STORAGE_SET_PER_BYTE, 10),
            ("storage_remove", STORAGE_REMOVE_BASE, 5_000),
            ("storage_remove per byte", STORAGE_REMOVE_PER_BYTE, 1),
            ("the environment queries", ENV_QUERY, 100),
            ("btc_header", BTC_HEADER, 2_000),
            ("asset_balance", ASSET_BALANCE, 2_000),
            ("asset_transfer", ASSET_TRANSFER, 10_000),
            ("asset_burn", ASSET_BURN, 10_000),
            ("create_contract", CREATE_CONTRACT_BASE, 100_000),
            ("asset_issue", ASSET_ISSUE_BASE, 20_000),
            ("asset_issue per byte", ASSET_ISSUE_PER_BYTE, 1),
            ("asset_create", ASSET_CREATE_BASE, 20_000),
            ("asset_create per byte", ASSET_CREATE_PER_BYTE, 1),
            ("asset_freeze and asset_allow", ASSET_SET_STATUS, 10_000),
            ("asset_pause", ASSET_PAUSE, 10_000),
            ("asset_renounce", ASSET_RENOUNCE, 10_000),
            ("asset_policy", ASSET_POLICY, 2_000),
            ("asset_status", ASSET_STATUS, 2_000),
            ("call_contract", CALL_CONTRACT_BASE, 20_000),
            (
                "call_contract per byte of input",
                CALL_CONTRACT_PER_INPUT_BYTE,
                1,
            ),
            ("attached funds per asset", FUNDS_PER_ASSET, 10_000),
            ("call_funds", CALL_FUNDS_BASE, 100),
            ("call_funds per byte", CALL_FUNDS_PER_BYTE, 1),
            ("native_tx", NATIVE_TX_BASE, 2_000),
            ("native_tx per byte", NATIVE_TX_PER_BYTE, 1),
            ("emit_event", EMIT_EVENT_BASE, 1_000),
            ("emit_event per byte", EMIT_EVENT_PER_BYTE, 5),
            ("sha256", SHA256_BASE, 500),
            ("sha256 per byte", SHA256_PER_BYTE, 1),
            ("verify_schnorr", VERIFY_SCHNORR, 50_000),
        ];
        for (name, value, abi) in table {
            assert_eq!(value, abi, "{name}");
        }
    }
}
