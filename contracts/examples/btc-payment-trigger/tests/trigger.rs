//! Tests of the BTC payment trigger contract on the SDK's in-memory mock host: synthetic, self-consistent transactions /
//! Merkle trees / block headers.
#![cfg(not(target_arch = "wasm32"))]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use btc_payment_trigger::beneficiary_commitment;
use ksana_contract_sdk::btc::BlockHeader;
use ksana_contract_sdk::mock::{self, Failure};
use ksana_contract_sdk::{self as sdk, decode, encode, AssetMetaInput};

const DEPLOYER: [u8; 32] = [0xde; 32];
const CLAIMER: [u8; 32] = [0xc1; 32];
const BENEFICIARY: [u8; 32] = [0xbe; 32];
const SUB_ID: [u8; 32] = [0x5b; 32];
const RATE: u128 = 1_000;
const HEIGHT: u32 = 100;

fn watch_script() -> Vec<u8> {
    [&[0x00, 0x14][..], &[0x55; 20]].concat() // P2WPKH
}

fn change_script() -> Vec<u8> {
    [&[0x00, 0x14][..], &[0x66; 20]].concat()
}

fn meta() -> AssetMetaInput {
    AssetMetaInput {
        name: "Wrapped sats".to_string(),
        symbol: "WSAT".to_string(),
        decimals: 8,
    }
}

/// Transaction serialization without witness data: 1 input and the given outputs.
fn tx(outputs: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut t = 2u32.to_le_bytes().to_vec();
    t.push(1);
    t.extend_from_slice(&[0x11; 32]);
    t.extend_from_slice(&0u32.to_le_bytes());
    t.push(0);
    t.extend_from_slice(&u32::MAX.to_le_bytes());
    t.push(outputs.len() as u8);
    for (value, script) in outputs {
        t.extend_from_slice(&value.to_le_bytes());
        t.push(script.len() as u8);
        t.extend_from_slice(script);
    }
    t.extend_from_slice(&0u32.to_le_bytes());
    t
}

fn payment_tx() -> Vec<u8> {
    tx(&[
        (50_000, watch_script()),
        (0, beneficiary_commitment(&BENEFICIARY)),
        (12_345, change_script()),
    ])
}

/// Bitcoin Merkle tree: returns the root and the branch of every leaf (an odd level duplicates its last node).
fn merkle(leaves: &[[u8; 32]]) -> ([u8; 32], Vec<Vec<[u8; 32]>>) {
    let mut branches = vec![Vec::new(); leaves.len()];
    let mut positions: Vec<usize> = (0..leaves.len()).collect();
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        for (branch, pos) in branches.iter_mut().zip(positions.iter_mut()) {
            branch.push(level[*pos ^ 1]);
            *pos /= 2;
        }
        level = level
            .chunks(2)
            .map(|pair| sdk::sha256d(&[pair[0], pair[1]].concat()))
            .collect();
    }
    (level[0], branches)
}

fn header_with_root(merkle_root: [u8; 32]) -> [u8; 80] {
    BlockHeader {
        version: 0x2000_0000,
        prev_blockhash: [0x22; 32],
        merkle_root,
        time: 1_700_000_000,
        bits: 0x207f_ffff,
        nonce: 0,
    }
    .to_bytes()
}

/// Coinbase transaction (without witness data): one input with a null previous output (BIP34 height), one output.
fn coinbase_tx() -> Vec<u8> {
    let mut t = 2u32.to_le_bytes().to_vec();
    t.push(1);
    t.extend_from_slice(&[0; 32]);
    t.extend_from_slice(&u32::MAX.to_le_bytes());
    t.push(4);
    t.extend_from_slice(&[0x03, 0x64, 0x00, 0x00]);
    t.extend_from_slice(&u32::MAX.to_le_bytes());
    t.push(1);
    t.extend_from_slice(&5_000_000_000u64.to_le_bytes());
    t.push(1);
    t.push(0x51);
    t.extend_from_slice(&0u32.to_le_bytes());
    t
}

/// Inclusion proof of a transaction in the block, together with the coinbase proof that pins the depth of the Merkle tree
/// (D-072).
#[derive(Clone, Debug)]
struct Proof {
    index: u32,
    branch: Vec<[u8; 32]>,
    coinbase: Vec<u8>,
    coinbase_branch: Vec<[u8; 32]>,
}

/// Puts the block header at HEIGHT (anchor height HEIGHT + 6): transaction 0 is the coinbase, followed by `txids` in order.
/// Returns the branches of all transactions (coinbase included) and the Merkle root.
fn put_txids(txids: &[[u8; 32]]) -> (Vec<Vec<[u8; 32]>>, [u8; 32]) {
    let mut leaves = vec![sdk::sha256d(&coinbase_tx())];
    leaves.extend_from_slice(txids);
    let (root, branches) = merkle(&leaves);
    mock::set_btc_header(HEIGHT, header_with_root(root));
    mock::set_btc_anchor_height(HEIGHT + 6);
    (branches, root)
}

/// Proof of transaction `index` (`branches` comes from [`put_txids`]).
fn proof_at(branches: &[Vec<[u8; 32]>], index: usize) -> Proof {
    Proof {
        index: u32::try_from(index).unwrap(),
        branch: branches[index].clone(),
        coinbase: coinbase_tx(),
        coinbase_branch: branches[0].clone(),
    }
}

/// A block with 5 transactions (transaction 0 is the coinbase); `raw_tx` is transaction 3.
fn put_block(raw_tx: &[u8]) -> Proof {
    let mut txids: Vec<[u8; 32]> = (1u8..5).map(|i| sdk::sha256d(&[i; 100])).collect();
    txids[2] = sdk::sha256d(raw_tx);
    proof_at(&put_txids(&txids).0, 3)
}

fn deploy(rate: u128) -> [u8; 32] {
    mock::reset();
    mock::deploy(
        DEPLOYER,
        0,
        b"btc-payment-trigger",
        btc_payment_trigger::dispatch,
        &encode(&(watch_script(), SUB_ID, meta(), rate)),
    )
    .unwrap()
}

fn claim(
    contract: [u8; 32],
    raw_tx: &[u8],
    proof: &Proof,
    vout: u32,
    beneficiary: [u8; 32],
) -> Result<u128, Failure> {
    let args = encode(&(
        HEIGHT,
        raw_tx.to_vec(),
        proof.index,
        proof.branch.clone(),
        proof.coinbase.clone(),
        proof.coinbase_branch.clone(),
        vout,
        beneficiary,
    ));
    mock::call(CLAIMER, contract, "claim", &args).map(|out| decode(&out).unwrap())
}

fn revert_reason(result: Result<u128, Failure>) -> Vec<u8> {
    match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    }
}

fn is_claimed(contract: [u8; 32], txid: [u8; 32], vout: u32) -> bool {
    decode(&mock::call(CLAIMER, contract, "is_claimed", &encode(&(txid, vout))).unwrap()).unwrap()
}

#[test]
fn claim_issues_the_contract_asset() {
    let contract = deploy(RATE);
    let raw = payment_tx();
    let proof = put_block(&raw);
    let amount = claim(contract, &raw, &proof, 0, BENEFICIARY).unwrap();
    assert_eq!(amount, 50_000 * RATE);

    let asset: [u8; 32] = decode(&mock::call(CLAIMER, contract, "asset_id", &[]).unwrap()).unwrap();
    assert_eq!(asset, sdk::asset_id(&contract, &SUB_ID));
    assert_eq!(mock::balance(&asset, &BENEFICIARY), 50_000 * RATE);
    assert_eq!(mock::balance(&asset, &CLAIMER), 0);
    let info = mock::asset_info(&asset).unwrap();
    assert_eq!(info.name, "Wrapped sats");
    assert_eq!(info.decimals, 8);
    assert_eq!(info.total_supply, 50_000 * RATE);
    assert_eq!(info.issuer, contract);

    let txid = sdk::sha256d(&raw);
    assert!(is_claimed(contract, txid, 0));
    assert!(!is_claimed(contract, txid, 2));
    let event = mock::events().last().unwrap().clone();
    assert_eq!(event.contract, contract);
    assert_eq!(
        event.data,
        encode(&("claim", txid, 0u32, BENEFICIARY, asset, 50_000 * RATE))
    );
}

#[test]
fn an_output_can_only_be_claimed_once() {
    let contract = deploy(RATE);
    let raw = payment_tx();
    let proof = put_block(&raw);
    claim(contract, &raw, &proof, 0, BENEFICIARY).unwrap();
    assert_eq!(
        revert_reason(claim(contract, &raw, &proof, 0, BENEFICIARY)),
        b"output already claimed".to_vec()
    );
    let asset = sdk::asset_id(&contract, &SUB_ID);
    assert_eq!(mock::balance(&asset, &BENEFICIARY), 50_000 * RATE);
}

#[test]
fn header_must_be_deep_enough() {
    let contract = deploy(RATE);
    let raw = payment_tx();
    let proof = put_block(&raw);
    mock::set_btc_anchor_height(HEIGHT + 5);
    let result = claim(contract, &raw, &proof, 0, BENEFICIARY);
    match result {
        Err(Failure::Trap(reason)) => assert!(reason.contains("deep"), "{reason}"),
        other => panic!("expected a trap, got {other:?}"),
    }
    assert!(!is_claimed(contract, sdk::sha256d(&raw), 0));
    // claimable once deep enough.
    mock::set_btc_anchor_height(HEIGHT + 6);
    assert!(claim(contract, &raw, &proof, 0, BENEFICIARY).is_ok());
}

#[test]
fn invalid_inclusion_proofs_are_rejected() {
    let contract = deploy(RATE);
    let raw = payment_tx();
    let proof = put_block(&raw);
    let invalid = b"transaction is not included in the block".to_vec();
    let rejected = |proof: Proof| revert_reason(claim(contract, &raw, &proof, 0, BENEFICIARY));
    assert_eq!(
        rejected(Proof {
            index: proof.index - 1,
            ..proof.clone()
        }),
        invalid
    );
    assert_eq!(
        rejected(Proof {
            index: proof.index + 8,
            ..proof.clone()
        }),
        invalid
    );
    let mut tampered = proof.clone();
    tampered.branch[0][5] ^= 1;
    assert_eq!(rejected(tampered), invalid);
    let mut short = proof.clone();
    short.branch.truncate(2);
    assert_eq!(rejected(short), invalid);
    // coinbase proof: a tampered branch, a length different from the transaction's branch, something other than the
    // coinbase given at position 0
    let mut tampered = proof.clone();
    tampered.coinbase_branch[1][0] ^= 1;
    assert_eq!(rejected(tampered), invalid);
    let mut short = proof.clone();
    short.coinbase_branch.truncate(2);
    assert_eq!(rejected(short), invalid);
    assert_eq!(
        rejected(Proof {
            coinbase: raw.clone(),
            ..proof.clone()
        }),
        invalid
    );
    // changing the transaction (e.g. the amount) changes the txid and invalidates the proof.
    let mut inflated = raw.clone();
    let value_offset = 4 + 1 + 32 + 4 + 1 + 4 + 1;
    inflated[value_offset + 3] = 0x7f;
    assert_eq!(
        revert_reason(claim(contract, &inflated, &proof, 0, BENEFICIARY)),
        invalid
    );
    assert!(!is_claimed(contract, sdk::sha256d(&raw), 0));
}

/// The block contains a real 64-byte transaction `T = L ‖ txid(F)`: treating T as an internal node, the payment F that never
/// happened has a branch that computes to the block's Merkle root but is one level deeper than the real leaves
/// (CVE-2017-12842). F itself is not 64 bytes; the coinbase proof pins the tree depth and the claim is rejected (D-072).
#[test]
fn a_payment_grafted_below_a_64_byte_transaction_is_rejected() {
    let contract = deploy(RATE);
    let fake = payment_tx();
    let left = [0x77u8; 32];
    let t = [left.as_slice(), sdk::sha256d(&fake).as_slice()].concat();
    assert_eq!(t.len(), 64);
    // block: [coinbase, T, another transaction]
    let (branches, root) = put_txids(&[sdk::sha256d(&t), sdk::sha256d(&[9u8; 100])]);
    let grafted = Proof {
        index: (1 << 1) | 1,
        branch: [vec![left], branches[1].clone()].concat(),
        coinbase: coinbase_tx(),
        coinbase_branch: branches[0].clone(),
    };
    assert!(sdk::btc::verify_merkle_branch(
        &sdk::sha256d(&fake),
        grafted.index,
        &grafted.branch,
        &root
    ));
    let invalid = b"transaction is not included in the block".to_vec();
    assert_eq!(
        revert_reason(claim(contract, &fake, &grafted, 0, BENEFICIARY)),
        invalid
    );
    // padding the coinbase branch by one level does not produce the same depth either
    let padded = Proof {
        coinbase_branch: [vec![[0u8; 32]], branches[0].clone()].concat(),
        ..grafted.clone()
    };
    assert_eq!(
        revert_reason(claim(contract, &fake, &padded, 0, BENEFICIARY)),
        invalid
    );
    assert!(!is_claimed(contract, sdk::sha256d(&fake), 0));
}

#[test]
fn output_must_pay_the_watched_script() {
    let contract = deploy(RATE);
    let raw = payment_tx();
    let proof = put_block(&raw);
    let wrong = b"output does not pay the watched script".to_vec();
    assert_eq!(
        revert_reason(claim(contract, &raw, &proof, 2, BENEFICIARY)),
        wrong
    );
    assert_eq!(
        revert_reason(claim(contract, &raw, &proof, 1, BENEFICIARY)),
        wrong
    );
    assert_eq!(
        revert_reason(claim(contract, &raw, &proof, 3, BENEFICIARY)),
        b"no such output".to_vec()
    );
}

#[test]
fn beneficiary_must_be_committed_in_the_transaction() {
    let contract = deploy(RATE);
    let raw = payment_tx();
    let proof = put_block(&raw);
    // a front-runner replaces the beneficiary with themselves: the transaction carries no commitment to them.
    assert_eq!(
        revert_reason(claim(contract, &raw, &proof, 0, CLAIMER)),
        b"transaction does not commit to the beneficiary".to_vec()
    );
    // a payment without an OP_RETURN commitment cannot be claimed.
    let bare = tx(&[(50_000, watch_script())]);
    let proof = put_block(&bare);
    assert_eq!(
        revert_reason(claim(contract, &bare, &proof, 0, BENEFICIARY)),
        b"transaction does not commit to the beneficiary".to_vec()
    );
}

#[test]
fn several_payments_in_one_transaction() {
    let contract = deploy(RATE);
    let raw = tx(&[
        (10, watch_script()),
        (0, beneficiary_commitment(&BENEFICIARY)),
        (20, watch_script()),
    ]);
    // a block with only the coinbase and this payment: the branch has a single level, the payment is transaction 1.
    let proof = proof_at(&put_txids(&[sdk::sha256d(&raw)]).0, 1);
    assert_eq!(proof.branch.len(), 1);
    assert_eq!(
        claim(contract, &raw, &proof, 0, BENEFICIARY).unwrap(),
        10 * RATE
    );
    assert_eq!(
        claim(contract, &raw, &proof, 2, BENEFICIARY).unwrap(),
        20 * RATE
    );
    let asset = sdk::asset_id(&contract, &SUB_ID);
    assert_eq!(mock::asset_info(&asset).unwrap().total_supply, 30 * RATE);
}

#[test]
fn zero_value_and_overflow_are_rejected() {
    let contract = deploy(u128::MAX);
    let raw = payment_tx();
    let proof = put_block(&raw);
    assert_eq!(
        revert_reason(claim(contract, &raw, &proof, 0, BENEFICIARY)),
        b"arithmetic overflow".to_vec()
    );

    // an output paying the watched script with amount 0.
    let contract = deploy(RATE);
    let raw = tx(&[
        (0, watch_script()),
        (0, beneficiary_commitment(&BENEFICIARY)),
    ]);
    let proof = put_block(&raw);
    assert_eq!(
        revert_reason(claim(contract, &raw, &proof, 0, BENEFICIARY)),
        b"output value is zero".to_vec()
    );
}

#[test]
fn init_validates_parameters() {
    mock::reset();
    let deploy_with = |nonce: u64, script: Vec<u8>, meta: AssetMetaInput, rate: u128| {
        mock::deploy(
            DEPLOYER,
            nonce,
            b"btc-payment-trigger",
            btc_payment_trigger::dispatch,
            &encode(&(script, SUB_ID, meta, rate)),
        )
    };
    let reason = |result: Result<[u8; 32], Failure>| match result {
        Err(Failure::Revert { reason, .. }) => reason,
        other => panic!("expected a revert, got {other:?}"),
    };
    assert_eq!(
        reason(deploy_with(0, vec![], meta(), RATE)),
        b"watch_script must not be empty".to_vec()
    );
    assert_eq!(
        reason(deploy_with(1, watch_script(), meta(), 0)),
        b"units_per_sat must be positive".to_vec()
    );
    for (name, symbol, decimals) in [
        ("x".repeat(65), "X", 0),
        (String::new(), "X", 0),
        ("x".to_string(), "XXXXXXXXXXXXXXXXX", 0),
        ("x".to_string(), "", 0),
        ("x".to_string(), "X", 19),
    ] {
        let meta = AssetMetaInput {
            name,
            symbol: symbol.to_string(),
            decimals,
        };
        assert_eq!(
            reason(deploy_with(2, watch_script(), meta, RATE)),
            b"asset meta breaks the host's limits".to_vec()
        );
    }
    assert!(deploy_with(3, watch_script(), meta(), RATE).is_ok());
}
