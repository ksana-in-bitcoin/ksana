//! Example contract: payout triggered by a BTC payment (SPEC §9.4, §9.5).
//!
//! The deployer names a watched locking script `watch_script`. Once anyone has paid it on BTC and the transaction is
//! confirmed deep enough, anyone can submit the transaction with a Merkle proof and call `claim`: the contract uses
//! `btc_header(height)` (the host only returns headers at depth `BTC_QUERY_DEPTH` or more) to verify that the block
//! includes the transaction (the block's coinbase transaction and its proof are submitted as well, to pin the depth of
//! the Merkle tree, see `ksana_contract_sdk::btc::verify_tx_inclusion`) and that output `vout` pays `watch_script`,
//! then issues `amount (sats) × units_per_sat` of the contract's asset to `beneficiary`. Each output (txid, vout) can be
//! claimed only once.
//!
//! **Beneficiary binding**: the transaction must have an output whose locking script is exactly
//! `OP_RETURN <32-byte beneficiary>` (`0x6a 0x20 ‖ beneficiary`, 34 bytes). Otherwise anyone who sees the payment
//! (including the producer) could front-run the claim with themselves as the beneficiary.
//!
//! Parameters and return values are Borsh-encoded. The parameters form a tuple in the order of the table below (a single
//! parameter is encoded as itself; no parameters means empty bytes):
//!
//! | Method | Parameters | Returns |
//! | --- | --- | --- |
//! | `init` (at deployment) | `(watch_script: Vec<u8>, sub_id: [u8; 32], meta: AssetMetaInput, units_per_sat: u128)` | `()` |
//! | `claim` | `(height: u32, raw_tx: Vec<u8>, tx_index: u32, branch: Vec<[u8; 32]>, coinbase_tx: Vec<u8>, coinbase_branch: Vec<[u8; 32]>, vout: u32, beneficiary: [u8; 32])` | `u128` (amount issued) |
//! | `asset_id` | none | `[u8; 32]` |
//! | `is_claimed` | `(txid: [u8; 32], vout: u32)` | `bool` |
//!
//! `AssetMetaInput` is encoded as `(name: String, symbol: String, decimals: u8)`: a name of 1 to 64 bytes, a symbol of
//! 1 to 16 bytes and at most 18 decimals (D-121). `raw_tx` and `coinbase_tx` are both transaction
//! serializations **without witness data** (for a segwit transaction, strip the marker, the flag and the witnesses first);
//! `txid` and the branches are in Bitcoin's internal byte order (D-001), a branch starts at the leaf side, and bit i of
//! `tx_index` being 1 means that the current node at level i is the right child; `coinbase_branch` is the branch of the
//! coinbase (position 0) and must have the same length as `branch` (D-072).
//!
//! Events (Borsh): `("claim", txid: [u8; 32], vout: u32, beneficiary: [u8; 32], asset_id: [u8; 32], amount: u128)`.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use ksana_contract_sdk::{self as sdk, btc, storage, AssetMetaInput, CallResult};

sdk::entry!(dispatch);

const KEY_CONFIG: &[u8] = b"config";
const PREFIX_CLAIMED: &[u8] = b"c";

/// The host's limits on the asset meta (ABI §3, D-121): the name's and the symbol's bytes and the decimals.
const MAX_NAME_BYTES: usize = 64;
const MAX_SYMBOL_BYTES: usize = 16;
const MAX_DECIMALS: u8 = 18;
/// Locking-script prefix of the beneficiary commitment output: `OP_RETURN OP_PUSHBYTES_32`.
const COMMITMENT_PREFIX: [u8; 2] = [0x6a, 0x20];

const ERR_ALREADY_INITIALIZED: &[u8] = b"already initialized";
const ERR_NOT_INITIALIZED: &[u8] = b"not initialized";
const ERR_EMPTY_SCRIPT: &[u8] = b"watch_script must not be empty";
const ERR_ZERO_RATE: &[u8] = b"units_per_sat must be positive";
const ERR_BAD_META: &[u8] = b"asset meta breaks the host's limits";
const ERR_MALFORMED_HEADER: &[u8] = b"malformed btc header";
const ERR_INVALID_PROOF: &[u8] = b"transaction is not included in the block";
const ERR_MALFORMED_TX: &[u8] = b"malformed transaction";
const ERR_NO_SUCH_OUTPUT: &[u8] = b"no such output";
const ERR_WRONG_SCRIPT: &[u8] = b"output does not pay the watched script";
const ERR_ZERO_VALUE: &[u8] = b"output value is zero";
const ERR_NO_COMMITMENT: &[u8] = b"transaction does not commit to the beneficiary";
const ERR_ALREADY_CLAIMED: &[u8] = b"output already claimed";
const ERR_OVERFLOW: &[u8] = b"arithmetic overflow";

/// Deployment parameters: `(watch_script, sub_id, meta, units_per_sat)`.
type Config = (Vec<u8>, [u8; 32], AssetMetaInput, u128);

/// Parameters of `claim`: `(height, raw_tx, tx_index, branch, coinbase_tx, coinbase_branch, vout, beneficiary)`.
type ClaimArgs = (
    u32,
    Vec<u8>,
    u32,
    Vec<[u8; 32]>,
    Vec<u8>,
    Vec<[u8; 32]>,
    u32,
    [u8; 32],
);

/// Inclusion proof of a BTC transaction: the block height, the transaction, its position and branch, plus the coinbase
/// transaction of the same block and its branch (pins the tree depth, D-072).
pub struct InclusionProof<'a> {
    /// Block height (read with `btc_header`).
    pub height: u32,
    /// Transaction serialization without witness data.
    pub raw_tx: &'a [u8],
    /// Position of the transaction in the block.
    pub tx_index: u32,
    /// Merkle branch of the transaction.
    pub branch: &'a [[u8; 32]],
    /// Coinbase transaction of the same block (without witness data).
    pub coinbase_tx: &'a [u8],
    /// Merkle branch of the coinbase (position 0).
    pub coinbase_branch: &'a [[u8; 32]],
}

/// Dispatch function of the contract entry point.
pub fn dispatch(method: &str, args: &[u8]) -> CallResult {
    match method {
        "init" => {
            let (watch_script, sub_id, meta, units_per_sat) = sdk::decode(args)?;
            init(watch_script, sub_id, meta, units_per_sat)?;
            sdk::ok(&())
        }
        "claim" => {
            let (height, raw_tx, tx_index, branch, coinbase_tx, coinbase_branch, vout, beneficiary): ClaimArgs =
                sdk::decode(args)?;
            let proof = InclusionProof {
                height,
                raw_tx: &raw_tx,
                tx_index,
                branch: &branch,
                coinbase_tx: &coinbase_tx,
                coinbase_branch: &coinbase_branch,
            };
            sdk::ok(&claim(&proof, vout, &beneficiary)?)
        }
        "asset_id" => {
            sdk::decode::<()>(args)?;
            sdk::ok(&asset_id()?)
        }
        "is_claimed" => {
            let (txid, vout) = sdk::decode(args)?;
            sdk::ok(&is_claimed(&txid, vout))
        }
        _ => sdk::revert("unknown method"),
    }
}

fn config() -> Result<Config, Vec<u8>> {
    storage::read(KEY_CONFIG)?.ok_or_else(|| ERR_NOT_INITIALIZED.to_vec())
}

fn claimed_key(txid: &[u8; 32], vout: u32) -> Vec<u8> {
    [
        PREFIX_CLAIMED,
        txid.as_slice(),
        vout.to_le_bytes().as_slice(),
    ]
    .concat()
}

/// Beneficiary commitment: the locking script `OP_RETURN <beneficiary>`.
pub fn beneficiary_commitment(beneficiary: &[u8; 32]) -> Vec<u8> {
    [COMMITMENT_PREFIX.as_slice(), beneficiary.as_slice()].concat()
}

/// `init(watch_script, sub_id, meta, units_per_sat)`.
pub fn init(
    watch_script: Vec<u8>,
    sub_id: [u8; 32],
    meta: AssetMetaInput,
    units_per_sat: u128,
) -> Result<(), Vec<u8>> {
    if storage::get(KEY_CONFIG).is_some() {
        return Err(ERR_ALREADY_INITIALIZED.to_vec());
    }
    if watch_script.is_empty() {
        return Err(ERR_EMPTY_SCRIPT.to_vec());
    }
    if units_per_sat == 0 {
        return Err(ERR_ZERO_RATE.to_vec());
    }
    // a meta over the host's limits would make every later `asset_issue` trap; reject it at deployment.
    if !(1..=MAX_NAME_BYTES).contains(&meta.name.len())
        || !(1..=MAX_SYMBOL_BYTES).contains(&meta.symbol.len())
        || meta.decimals > MAX_DECIMALS
    {
        return Err(ERR_BAD_META.to_vec());
    }
    let config: Config = (watch_script, sub_id, meta, units_per_sat);
    storage::write(KEY_CONFIG, &config);
    Ok(())
}

/// `claim(height, raw_tx, tx_index, branch, coinbase_tx, coinbase_branch, vout, beneficiary) -> amount`.
pub fn claim(
    proof: &InclusionProof<'_>,
    vout: u32,
    beneficiary: &[u8; 32],
) -> Result<u128, Vec<u8>> {
    let (watch_script, sub_id, meta, units_per_sat) = config()?;
    // the host traps when the depth is insufficient or the header does not exist; the whole operation fails.
    let header = btc::BlockHeader::parse(&sdk::btc_header(proof.height))
        .ok_or_else(|| ERR_MALFORMED_HEADER.to_vec())?;
    let raw_tx = proof.raw_tx;
    let txid = btc::verify_tx_inclusion(
        &header,
        raw_tx,
        proof.tx_index,
        proof.branch,
        proof.coinbase_tx,
        proof.coinbase_branch,
    )
    .ok_or_else(|| ERR_INVALID_PROOF.to_vec())?;
    let outputs = btc::parse_outputs(raw_tx).ok_or_else(|| ERR_MALFORMED_TX.to_vec())?;
    let output = usize::try_from(vout)
        .ok()
        .and_then(|index| outputs.get(index))
        .ok_or_else(|| ERR_NO_SUCH_OUTPUT.to_vec())?;
    if output.script_pubkey != watch_script.as_slice() {
        return Err(ERR_WRONG_SCRIPT.to_vec());
    }
    if output.value == 0 {
        return Err(ERR_ZERO_VALUE.to_vec());
    }
    let commitment = beneficiary_commitment(beneficiary);
    if !outputs
        .iter()
        .any(|out| out.script_pubkey == commitment.as_slice())
    {
        return Err(ERR_NO_COMMITMENT.to_vec());
    }
    let key = claimed_key(&txid, vout);
    if storage::get(&key).is_some() {
        return Err(ERR_ALREADY_CLAIMED.to_vec());
    }
    let amount = u128::from(output.value)
        .checked_mul(units_per_sat)
        .ok_or_else(|| ERR_OVERFLOW.to_vec())?;
    storage::write(&key, &(*beneficiary, amount));
    let asset = sdk::asset_issue(&sub_id, &meta, beneficiary, amount);
    sdk::emit(&("claim", txid, vout, *beneficiary, asset, amount));
    Ok(amount)
}

/// `asset_id() -> sha256(this contract's ID ‖ sub_id)`.
pub fn asset_id() -> Result<[u8; 32], Vec<u8>> {
    let (_, sub_id, _, _) = config()?;
    Ok(sdk::asset_id(&sdk::self_id(), &sub_id))
}

/// `is_claimed(txid, vout)`.
pub fn is_claimed(txid: &[u8; 32], vout: u32) -> bool {
    storage::get(&claimed_key(txid, vout)).is_some()
}
